//! The `DjogiContext` type — carries either a pooled handle or an active transaction.
//!
//! Per Phase 4 v3 specification, `DjogiContext` **replaces** the `E: Executor` generic
//! on every `Model` CRUD and `QuerySet` method signature. This change unifies the API:
//! the same method can be called against a pool or inside a transaction without
//! reborrows or type juggling.
//!
//! # Context variants
//!
//! A context is one of:
//! - **Pool**: backed by a `DjogiPool` — each operation checks out a connection, runs
//!   the query, and returns the connection to the pool.
//! - **Transaction**: an active `PgConnection` with an open transaction — all
//!   operations share the same logical transaction until `commit()` or `rollback()`
//!   is called.
//!
//! # Execution dispatch pattern
//!
//! CRUD methods and QuerySet terminals that today take `&mut DjogiContext` acquire a
//! `PgConnection` from the pool (Pool path) or reuse the existing connection
//! (Transaction path). The inline match on [`ContextInner`] is the dispatch
//! mechanism:
//!
//! ```ignore
//! let rows = ctx.query_all(sql, params).await?;
//! ```
//!
//! Two variants = two match arms = negligible overhead.
//!
//! # Savepoint depth and nesting
//!
//! When `atomic()` (Phase 4 Task 1) opens a transaction inside another transaction,
//! Postgres transparently converts it to a savepoint. The `savepoint_depth` field
//! tracks how many nested `atomic()` calls have been made (0 = root transaction or
//! pool, N = N savepoints). The framework uses this to auto-name savepoints as
//! `sp_<depth>` without user involvement.
//!
//! # On-commit callbacks
//!
//! Callbacks registered via `.on_commit()` fire after a successful `commit()`.
//! They are useful for post-transaction side effects (cache invalidation,
//! outbox polling, audit logging). Callback errors are logged but do not fail
//! the commit itself (per the spec resolution). Callbacks are FIFO.
//!
//! # Drain points
//!
//! Registered callbacks are consumed by exactly two paths:
//!
//! - [`DjogiContext::commit`] — the low-level tx-backed commit drains the
//!   queue after the underlying commit succeeds, runs each callback in
//!   FIFO order, and logs any callback error via `tracing::error!` without
//!   unwinding the caller.
//! - `atomic()` (Phase 4 Task 1) — once landed, the canonical entry point
//!   for application code; wraps the same drain-after-commit semantics but
//!   also handles nested savepoints.
//!
//! Calls to [`DjogiContext::on_commit`] on a **pool-backed** context (no
//! `atomic()` scope, no surrounding transaction) are an audit-warn no-op:
//! the callback is **not** queued, and a `#[track_caller] tracing::warn!`
//! event with the grep-able token `djogi::on_commit::pool_backed_drop`
//! fires once per `on_commit` call so log scrapers catch every silent-drop
//! site. The recommended fix is to wrap the CRUD chain in
//! [`djogi::transaction::atomic`](crate::transaction::atomic) (or open a
//! transaction explicitly via [`DjogiContext::begin`]) so the
//! [`commit`](DjogiContext::commit) drain step actually fires. The warn
//! is **single-shot per `on_commit` call** — it does not amplify per row
//! or per drain step.

use crate::auth::AuthContext;
use crate::pg::connection::PgConnection;
use crate::pg::pool::DjogiPool;
use crate::{DbError, DjogiError};
use futures::FutureExt;
use postgres_types::ToSql;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use tokio_postgres::Row;

/// Type alias for an async callback that fires after commit.
///
/// Represents a boxed closure that returns an async result. Used for the on-commit
/// callback stack to reduce type complexity in `DjogiContext`.
type OnCommitCallback = Box<
    dyn FnOnce() -> Pin<Box<dyn std::future::Future<Output = Result<(), DjogiError>> + Send>>
        + Send,
>;

/// The execution context for all CRUD operations.
///
/// Carries either a pooled handle or an active transaction + savepoint tracking.
/// Replaces the `E: Executor` generic on `Model` and `QuerySet` signatures.
pub struct DjogiContext {
    /// Internal variant: either a pool or a transaction.
    inner: ContextInner,

    /// Savepoint depth: 0 = root transaction or pool, N = N nested `atomic()` calls.
    /// Used by the framework to auto-generate savepoint names.
    savepoint_depth: u32,

    /// FIFO stack of callbacks to fire after a successful commit.
    /// Each callback is a boxed async closure that returns `Result<(), DjogiError>`.
    /// Errors are logged but do not fail the commit (Q9 resolution).
    on_commit: Vec<OnCommitCallback>,

    /// Whether [`set_tenant`](Self::set_tenant) has been called on this context.
    ///
    /// Set to `true` by `set_tenant`. Framework code can inspect this flag to
    /// detect missing tenant setup before performing tenant-scoped queries. Note
    /// that `SET LOCAL` (via `set_config`) is only meaningful inside an open
    /// transaction — callers must wrap tenant-scoped operations in `atomic()` for
    /// the GUC value to persist through the full query sequence.
    pub tenant_set: bool,

    /// Optional auth context attached via [`Self::with_auth`] (Phase 5.5 Task 1).
    ///
    /// `None` until the caller explicitly calls `with_auth` or
    /// `with_auth_insecurely`. CRUD operations and QuerySet terminals that
    /// require an authenticated caller check this field via [`Self::auth`].
    pub(crate) auth: Option<AuthContext>,

    /// When `true`, suppresses the "cross-tenant context" warn that fires
    /// when `auth.tenant_id.is_none()` on a tenant-keyed model. Set via
    /// [`DjogiContext::with_no_tenant_scope`] / [`Self::set_no_tenant_scope`].
    /// Phase 5.5 Task 11 addition.
    pub(crate) tenant_scope_suppressed: bool,

    /// The tenant id currently applied to this context via `set_tenant` /
    /// `ensure_tenant_set`, or `None` if no `SET LOCAL app.tenant_id = ...`
    /// has been issued on the current transaction. Used by
    /// [`Self::ensure_tenant_set`] to detect stale tenant scope: when auth
    /// changes inside an `atomic()` scope from `org_a` to `org_b`, the
    /// transaction-scoped GUC from the earlier `set_tenant("org_a")` is
    /// still in effect, so we re-issue `SET LOCAL` whenever the requested
    /// tid differs from the applied one. A plain `tenant_set: bool` short-
    /// circuit was insufficient because it let stale-tenant reads land
    /// silently across tenant boundaries inside one transaction (Phase 5.5
    /// Task 10 fixup — Codex stop-gate review of `f393a87`).
    pub(crate) applied_tenant_id: Option<String>,

    /// Cluster 8δ T7.4 — typed `Punnu<T>` pool registry for this context.
    ///
    /// Built once at construction time by walking `inventory::iter::<SassiBootHook>()`.
    /// Top-level contexts (`from_pool`, `from_connection`) each own a fresh
    /// `Arc<Sassi>`. `begin()` and `atomic(&mut pool_ctx, ...)` SHARE the
    /// parent's `Arc<Sassi>` (cache state is transaction-scope-agnostic — the
    /// registry does not track in-flight mutations, only provides access to the
    /// pools). `atomic(&pool, ...)` intentionally creates a fresh top-level
    /// transaction context because no parent `DjogiContext` was supplied.
    ///
    /// Access via [`Self::punnu::<T>()`]; the raw `Arc<Sassi>` is crate-private.
    pub(crate) sassi: Arc<sassi::Sassi>,
}

/// Snapshot of the auth-related mutable state on a [`DjogiContext`],
/// taken before entering a nested `atomic()` savepoint and restored on
/// rollback so the in-memory trackers stay in sync with Postgres's
/// actual GUC state after a savepoint rollback reverts the inner
/// `SET LOCAL`. Phase 5.5 phase-boundary fixup (Codex stop-gate review
/// of the full `phase-5-5-auth` branch).
#[doc(hidden)]
pub struct AuthStateSnapshot {
    pub(crate) auth: Option<AuthContext>,
    pub(crate) applied_tenant_id: Option<String>,
    pub(crate) tenant_set: bool,
    pub(crate) tenant_scope_suppressed: bool,
}

/// Internal enum selecting the active context variant.
///
/// `#[doc(hidden)] pub` because framework modules pattern-match on this
/// enum to dispatch to the connection. User code should go through the
/// execution helpers (`query_all`, `query_opt`, `query_one`, `execute`,
/// `batch_execute`) rather than reaching into the inner directly. The
/// `__` prefix and `#[doc(hidden)]` attribute are the social signal that
/// this type carries no stability guarantee.
// The `Transaction` variant holds a `PgConnection` (232 bytes, due to the
// `deadpool_postgres::Object` within). Boxing it would add an extra
// allocation per transaction — undesirable on the hot path. The size
// difference is intentional and expected: `Pool` holds only a clone of the
// pool handle (Arc<..>, 8 bytes); `Transaction` holds the full connection.
#[allow(clippy::large_enum_variant)]
#[doc(hidden)]
pub enum ContextInner {
    /// Pool-backed: acquires a connection per operation.
    Pool(DjogiPool),

    /// Transaction-backed: all operations share the same connection + transaction.
    ///
    /// The `PgConnection` wraps a checked-out pool connection. When this
    /// context is committed or rolled back, the connection is returned to the pool.
    Transaction(PgConnection),
}

/// Public-but-hidden alias of [`ContextInner`] for macro-generated code.
///
/// Macro-emitted CRUD bodies that pattern-match on the context's inner
/// variant to dispatch to the database now go through the execution
/// helpers (`ctx.query_one`, `ctx.execute`, etc.) rather than reaching
/// into this enum directly. This alias is kept for backward compatibility
/// during the T2 transition; T5 will remove the pattern-match exposure.
#[doc(hidden)]
pub type __ContextInnerForMacros = ContextInner;

impl DjogiContext {
    /// Create a context backed by a `DjogiPool`.
    ///
    /// # Example
    /// ```ignore
    /// let pool = DjogiPool::connect(url).await?;
    /// let mut ctx = DjogiContext::from_pool(pool);
    /// let user = User::create(&mut ctx, user).await?;
    /// ```
    pub fn from_pool(pool: DjogiPool) -> Self {
        Self::new(ContextInner::Pool(pool), Self::build_sassi())
    }

    fn new(inner: ContextInner, sassi: Arc<sassi::Sassi>) -> Self {
        DjogiContext {
            inner,
            savepoint_depth: 0,
            on_commit: Vec::new(),
            tenant_set: false,
            auth: None,
            tenant_scope_suppressed: false,
            applied_tenant_id: None,
            sassi,
        }
    }

    /// Create a context backed by an active `PgConnection` (transaction).
    ///
    /// Typically called by `atomic()` (Phase 4 Task 1) or by test / integration
    /// code that manages its own transaction boundaries. Production code
    /// should prefer [`atomic()`](crate::transaction::atomic) so on-commit
    /// callbacks dispatch correctly; this constructor is the low-level escape
    /// hatch for callers who really do need to hand-manage a transaction.
    pub fn from_connection(conn: PgConnection) -> Self {
        Self::new(ContextInner::Transaction(conn), Self::build_sassi())
    }

    /// Create a transaction-backed context that shares an existing
    /// `Arc<Sassi>`.
    ///
    /// This is the internal constructor for transaction scopes opened from an
    /// existing request context (`begin()` and `atomic(&mut pool_ctx, ...)`).
    /// It preserves the "DjogiContext is the cache/tenant boundary" contract:
    /// callers must already have chosen the parent context whose Sassi registry
    /// should flow into the transaction.
    ///
    /// This constructor shares only Sassi. Callers that need request metadata
    /// such as `auth` or `tenant_scope_suppressed` to flow into the transaction
    /// must copy that state explicitly.
    pub(crate) fn from_connection_with_sassi(conn: PgConnection, sassi: Arc<sassi::Sassi>) -> Self {
        Self::new(ContextInner::Transaction(conn), sassi)
    }

    /// Return the current savepoint depth (0 = root, N = N nested `atomic()` calls).
    pub fn savepoint_depth(&self) -> u32 {
        self.savepoint_depth
    }

    /// Increment savepoint depth by 1 (called when entering a nested `atomic()`).
    ///
    /// **Internal use only.** Used by the framework to manage savepoint nesting.
    #[allow(dead_code)]
    pub(crate) fn increment_savepoint_depth(&mut self) {
        self.savepoint_depth = self.savepoint_depth.saturating_add(1);
    }

    /// Decrement savepoint depth by 1 (called when exiting a nested `atomic()`).
    ///
    /// **Internal use only.** Used by the framework to manage savepoint nesting.
    #[allow(dead_code)]
    pub(crate) fn decrement_savepoint_depth(&mut self) {
        self.savepoint_depth = self.savepoint_depth.saturating_sub(1);
    }

    /// Get a reference to the inner pool if this context is pool-backed.
    ///
    /// Returns `Some(&pool)` iff the context was created via `from_pool()`.
    /// Returns `None` if this is a transaction context.
    pub(crate) fn pool(&self) -> Option<&DjogiPool> {
        match &self.inner {
            ContextInner::Pool(pool) => Some(pool),
            ContextInner::Transaction(_) => None,
        }
    }

    /// Clone the underlying pool when this context is pool-backed.
    ///
    /// Returns `None` for transaction-backed contexts. Use this when a
    /// higher-level API needs to own a pool handle, such as a long-lived
    /// delta-refresh subscription or a sibling context for cross-context
    /// cache tests. Ordinary CRUD code should keep passing `&mut
    /// DjogiContext` directly.
    pub fn share_pool(&self) -> Option<DjogiPool> {
        self.pool().cloned()
    }

    /// Get a mutable reference to the inner connection if this context is transaction-backed.
    ///
    /// Returns `Some(&mut conn)` iff the context was created via `from_connection()`.
    /// Returns `None` if this is a pool context.
    pub(crate) fn conn(&mut self) -> Option<&mut PgConnection> {
        match &mut self.inner {
            ContextInner::Pool(_) => None,
            ContextInner::Transaction(conn) => Some(conn),
        }
    }

    /// Crate-private mutable accessor for the context's inner variant.
    ///
    /// Used by every CRUD / QuerySet terminal in the framework to pattern-match
    /// on pool-vs-transaction at the database boundary.
    pub(crate) fn inner_mut(&mut self) -> &mut ContextInner {
        &mut self.inner
    }

    /// Public-but-hidden mutable accessor used by `#[derive(Model)]`-generated
    /// CRUD bodies to pattern-match on the context's inner variant.
    ///
    /// Not part of the stable API — prefixed with `__` and `#[doc(hidden)]`
    /// so the social signal is clear: downstream code should not call this
    /// directly.
    #[doc(hidden)]
    pub fn __inner_mut_for_macros(&mut self) -> &mut __ContextInnerForMacros {
        &mut self.inner
    }

    // -------------------------------------------------------------------------
    // Public-but-hidden execution helpers for macro-generated code.
    //
    // The `pub(crate)` helpers below are the framework-internal dispatch
    // surface.  These `__` variants expose the same functionality for code
    // generated by `#[model]`, which runs in user crates and therefore cannot
    // access `pub(crate)` members.  Naming and `#[doc(hidden)]` signal that
    // these carry no stability guarantee.
    // -------------------------------------------------------------------------

    /// Execute a query and return all rows. For use by macro-emitted code only.
    #[doc(hidden)]
    pub async fn __query_all_for_macros(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, DjogiError> {
        self.query_all(sql, params).await
    }

    /// Execute a query and return the first row, if any. For use by macro-emitted code only.
    #[doc(hidden)]
    pub async fn __query_opt_for_macros(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<Row>, DjogiError> {
        self.query_opt(sql, params).await
    }

    /// Execute a query and return exactly one row. For use by macro-emitted code only.
    #[doc(hidden)]
    pub async fn __query_one_for_macros(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Row, DjogiError> {
        self.query_one(sql, params).await
    }

    /// Execute a DML statement. For use by macro-emitted code only.
    #[doc(hidden)]
    pub async fn __execute_for_macros(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, DjogiError> {
        self.execute(sql, params).await
    }

    /// Public shim for macro-emitted code to read the tenant-scope
    /// suppression flag without reaching into the private field. Do not
    /// call from user code — no stability guarantee.
    #[doc(hidden)]
    pub fn __tenant_scope_suppressed_for_macros(&self) -> bool {
        self.tenant_scope_suppressed
    }

    /// Ensure `app.tenant_id` is set on this context. No-op when already
    /// set. For use by macro-emitted auto-tenant wiring only.
    ///
    /// Macro-emitted code runs in the user's crate and cannot reach
    /// `pub(crate)` members. This thin `pub` shim routes through the
    /// crate-internal [`Self::ensure_tenant_set`] that lives in
    /// `auth/context_ext.rs`.
    #[doc(hidden)]
    pub async fn __ensure_tenant_set_for_macros(
        &mut self,
        tenant_id: &str,
    ) -> Result<(), crate::DjogiError> {
        self.ensure_tenant_set(tenant_id).await
    }

    /// Snapshot the auth-related mutable state (`auth`, `applied_tenant_id`,
    /// `tenant_set`, `tenant_scope_suppressed`) so a nested `atomic()`
    /// savepoint can restore these fields on rollback to stay in sync
    /// with Postgres after the inner `SET LOCAL` is reverted.
    pub(crate) fn snapshot_auth_state(&self) -> AuthStateSnapshot {
        AuthStateSnapshot {
            auth: self.auth.clone(),
            applied_tenant_id: self.applied_tenant_id.clone(),
            tenant_set: self.tenant_set,
            tenant_scope_suppressed: self.tenant_scope_suppressed,
        }
    }

    /// Restore auth-related state captured by
    /// [`Self::snapshot_auth_state`]. Called by the nested `atomic()`
    /// path on rollback paths (closure returned Err, or panicked) so
    /// the in-memory trackers reflect the post-rollback GUC state that
    /// Postgres presents.
    pub(crate) fn restore_auth_state(&mut self, snapshot: AuthStateSnapshot) {
        self.auth = snapshot.auth;
        self.applied_tenant_id = snapshot.applied_tenant_id;
        self.tenant_set = snapshot.tenant_set;
        self.tenant_scope_suppressed = snapshot.tenant_scope_suppressed;
    }

    // -------------------------------------------------------------------------
    // Cluster 8δ T7.4 — Sassi registry helpers.
    // -------------------------------------------------------------------------

    /// Build a fresh `Arc<Sassi>` by walking the inventory of
    /// [`SassiBootHook`](crate::cache::SassiBootHook)s registered at link
    /// time via `inventory::submit!`.
    ///
    /// Every `#[model]` struct (except `pk = None`) emits one
    /// `inventory::submit!` in the macro-expansion phase. At runtime, this
    /// function iterates that inventory, calls each hook's `fn(&mut Sassi)`,
    /// and freezes the result into a shared `Arc`. Top-level `DjogiContext`
    /// constructors (`from_pool`, `from_connection`) call this once; inner
    /// contexts (`begin()`, `atomic(&mut pool_ctx, ...)`, and nested
    /// `atomic(&mut tx_ctx, ...)`) clone the parent's `Arc` instead of
    /// rebuilding. The compatibility `atomic(&pool, ...)` path remains a fresh
    /// top-level context by design, preserving the explicit context boundary.
    pub(crate) fn build_sassi() -> Arc<sassi::Sassi> {
        let mut s = sassi::Sassi::new();
        for hook in inventory::iter::<crate::cache::SassiBootHook>() {
            (hook.0)(&mut s);
        }
        Arc::new(s)
    }

    /// Return the registered [`Punnu<T>`](sassi::Punnu) for `T`.
    ///
    /// Returns `None` if:
    /// - `T` is a `pk = None` model (no `Cacheable` impl is auto-emitted,
    ///   so no boot hook ran for it), or
    /// - no `#[model]`-derived boot hook registered a pool for `T` (hand-
    ///   rolled `Cacheable` impls without an accompanying boot hook do not
    ///   appear in the registry).
    ///
    /// The same `Arc<Punnu<T>>` is returned on every call for the same `T`
    /// within one top-level `DjogiContext`. Contexts opened via `begin()` or
    /// `atomic(&mut pool_ctx, ...)` share the parent's `Arc<Sassi>`, so they
    /// return the same `Arc<Punnu<T>>` as well.
    ///
    /// # Cross-context contract (Cluster 8δ T7.4)
    ///
    /// Different top-level `DjogiContext` instances each build their own
    /// `Sassi`, so their `Punnu<T>` handles are DISTINCT — `Arc::ptr_eq`
    /// will return `false`. This is the "DjogiContext IS the tenant
    /// boundary" contract: two top-level contexts observe and mutate
    /// independent pools.
    ///
    /// To enforce that a received `Arc<Punnu<T>>` belongs to this context
    /// before passing it into a framework method, use
    /// [`DjogiContext::use_punnu`] (cluster 8δ T7.6).
    pub fn punnu<T>(&self) -> Option<Arc<sassi::Punnu<T>>>
    where
        T: crate::types::Cacheable + 'static,
    {
        self.sassi.pool::<T>()
    }

    /// Verify that `p` belongs to **this** context's `Sassi` registry and
    /// return a clone of it.
    ///
    /// This is the defensive accessor for adopter code that receives a
    /// `Punnu<T>` from some external source (e.g. from a different
    /// `DjogiContext`, or stored as application state) and needs to pass it to
    /// a framework method like [`QuerySet::cache`](crate::QuerySet::cache).
    /// Passing a `Punnu<T>` that was registered on a *different* context's
    /// `Sassi` is a cross-context misuse — the two contexts have independent
    /// cache registries (per the T7.4 "DjogiContext IS the tenant boundary"
    /// contract), so writes to one context's `Punnu` are never observable on
    /// the other.
    ///
    /// The check uses `Arc::ptr_eq` to compare the passed handle against the
    /// one registered in `self.sassi`. If they point at the same allocation,
    /// the handle belongs to this context; otherwise it was acquired from a
    /// different `DjogiContext`.
    ///
    /// # Mismatch behaviour (cfg-fork per `feedback_decision_priorities.md`)
    ///
    /// The decision matrix maps build target to the dominant priority axis:
    ///
    /// | Build | Axis | Behaviour |
    /// |---|---|---|
    /// | Debug | idiomatic Rust + scalability | `panic!` with a span-precise message |
    /// | Release | production stability | `tracing::error!` + empty `Punnu<T>` fallback |
    ///
    /// **Debug builds panic** because surfacing the misuse loudly during
    /// development lets the developer fix it before it reaches production.
    /// A silent fail-closed in debug mode would let cross-context bugs ship
    /// to prod undetected.
    ///
    /// **Release builds fail closed** because crashing a multi-tenant server
    /// on a single cross-context bug in cold-path code violates production
    /// stability. `tracing::error!` emits an observable signal for log
    /// scrapers; the empty `Punnu<T>` fallback is a live but isolated pool —
    /// reads return `None` (it is fresh-empty), and writes go into the
    /// fallback's own L1 cache only. They do NOT propagate to either the
    /// calling context's registered `Punnu<T>` or to the source `Arc` the
    /// caller passed in. The caller observes a normal `Arc<Punnu<T>>`
    /// surface but no cross-context state contamination is possible.
    ///
    /// # Returns
    ///
    /// - **Same-context path:** `Arc::clone(p)` — the same `Arc<Punnu<T>>`
    ///   that was registered at boot time for this context.
    /// - **Cross-context path (debug):** panics.
    /// - **Cross-context path (release):** a fresh empty
    ///   `Arc<Punnu<T>>` (reads return `None`; writes do not propagate).
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Correct: acquire from the same context.
    /// let p = ctx.punnu::<MyModel>().unwrap();
    /// let verified = ctx.use_punnu(&p); // returns Arc::clone(&p)
    ///
    /// // Misuse: acquire from a different context — debug build panics,
    /// // release build returns an empty Punnu.
    /// let p_other = ctx_b.punnu::<MyModel>().unwrap();
    /// let verified = ctx_a.use_punnu(&p_other); // BUG: cross-context
    /// ```
    ///
    /// # Spec anchor
    ///
    /// `docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md`
    /// §3 commit T7.6 — cross-context guard.
    pub fn use_punnu<T>(&self, p: &Arc<sassi::Punnu<T>>) -> Arc<sassi::Punnu<T>>
    where
        T: crate::types::Cacheable + 'static,
    {
        let registered = self.sassi.pool::<T>();
        let same_context = registered
            .as_ref()
            .map(|ours| Arc::ptr_eq(p, ours))
            .unwrap_or(false);

        if !same_context {
            if cfg!(debug_assertions) {
                panic!(
                    "cross-context Punnu access: this Punnu<{}> was not registered \
                     on this DjogiContext's Sassi. Each DjogiContext has its own \
                     cache registry per cluster 8δ T7.4 (Path X tenant boundary). \
                     Acquire the Punnu via ctx.punnu::<T>() on this same context.",
                    std::any::type_name::<T>(),
                );
            } else {
                tracing::error!(
                    target: "djogi::cache",
                    model = std::any::type_name::<T>(),
                    "cross-context Punnu access — returning empty Punnu fallback. \
                     The passed Punnu<T> is not bound to this DjogiContext: \
                     either it was acquired from a different context, or T \
                     was never registered on this context's boot inventory \
                     (e.g. pk = None or no #[derive(Model)]). Reads return \
                     None; writes go into the fallback's own L1 only. This \
                     is a release-build fail-closed; the caller should use \
                     ctx.punnu::<T>() on this context.",
                );
                return Arc::new(sassi::Punnu::<T>::builder().build());
            }
        }

        Arc::clone(p)
    }

    // -------------------------------------------------------------------------
    // Execution helpers — the new dispatch surface for query terminals.
    // -------------------------------------------------------------------------

    /// Execute a parameterised query and return all rows.
    ///
    /// On the pool path, acquires a connection for the duration of this call.
    /// On the transaction path, reuses the existing connection.
    pub(crate) async fn query_all(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, DjogiError> {
        match &mut self.inner {
            ContextInner::Pool(pool) => {
                let mut conn = pool.get().await?;
                conn.query(sql, params).await
            }
            ContextInner::Transaction(conn) => conn.query(sql, params).await,
        }
    }

    /// Execute a parameterised query and return the first row, if any.
    pub(crate) async fn query_opt(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<Row>, DjogiError> {
        match &mut self.inner {
            ContextInner::Pool(pool) => {
                let mut conn = pool.get().await?;
                conn.query_opt(sql, params).await
            }
            ContextInner::Transaction(conn) => conn.query_opt(sql, params).await,
        }
    }

    /// Execute a parameterised query and return exactly one row.
    ///
    /// Returns an error if zero or more than one row is returned.
    pub(crate) async fn query_one(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Row, DjogiError> {
        match &mut self.inner {
            ContextInner::Pool(pool) => {
                let mut conn = pool.get().await?;
                conn.query_one(sql, params).await
            }
            ContextInner::Transaction(conn) => conn.query_one(sql, params).await,
        }
    }

    /// Execute a parameterised DML statement and return the number of rows affected.
    pub(crate) async fn execute(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, DjogiError> {
        match &mut self.inner {
            ContextInner::Pool(pool) => {
                let mut conn = pool.get().await?;
                conn.execute(sql, params).await
            }
            ContextInner::Transaction(conn) => conn.execute(sql, params).await,
        }
    }

    /// Execute a simple (no-bind) SQL statement.
    ///
    /// Used for `BEGIN`, `COMMIT`, `ROLLBACK`, savepoint commands, and other
    /// control statements that carry no user-supplied values.
    #[allow(dead_code)] // Used by PgConnection directly in transaction.rs; may be wired up in T5.
    pub(crate) async fn batch_execute(&mut self, sql: &str) -> Result<(), DjogiError> {
        match &mut self.inner {
            ContextInner::Pool(pool) => {
                let mut conn = pool.get().await?;
                conn.batch_execute(sql).await
            }
            ContextInner::Transaction(conn) => conn.batch_execute(sql).await,
        }
    }

    // -------------------------------------------------------------------------
    // Transaction lifecycle.
    // -------------------------------------------------------------------------

    /// Commit the underlying transaction, consuming the context.
    ///
    /// Returns `Ok(())` if the context was transaction-backed and the commit
    /// succeeded. Returns `Err(DjogiError::Db(..))` if the commit failed or
    /// the context was pool-backed (pool contexts have no transaction to
    /// commit — calling `.commit()` on one is a caller error).
    ///
    /// # On-commit callbacks
    ///
    /// After the commit returns `Ok(())`, every callback registered via
    /// [`on_commit`](Self::on_commit) fires in FIFO order. ,
    /// callback errors are logged via `tracing::error!` but do NOT unwind the
    /// caller — a failing callback must not fail the commit, and subsequent
    /// callbacks still fire.
    ///
    /// If the underlying commit fails, the callbacks are dropped without
    /// running (the transaction did not commit, so post-commit side effects
    /// are inappropriate).
    pub async fn commit(self) -> Result<(), DjogiError> {
        let DjogiContext {
            inner, on_commit, ..
        } = self;

        match inner {
            ContextInner::Pool(_) => Err(DjogiError::Db(DbError::other(
                "DjogiContext::commit called on a pool-backed context",
            ))),
            ContextInner::Transaction(mut conn) => {
                conn.batch_execute("COMMIT").await?;
                drain_on_commit(on_commit).await;
                Ok(())
            }
        }
    }

    /// Roll back the underlying transaction, consuming the context.
    ///
    /// Returns `Ok(())` if the context was transaction-backed and the
    /// rollback succeeded. Returns `Err(DjogiError::Db(..))` if the rollback
    /// failed or the context was pool-backed.
    ///
    /// # On-commit callbacks
    ///
    /// Any callbacks registered via [`on_commit`](Self::on_commit) during
    /// this transaction are discarded (not fired). Post-commit side effects
    /// only make sense against a successful commit; rollback explicitly
    /// throws them away.
    pub async fn rollback(mut self) -> Result<(), DjogiError> {
        // Discard queued callbacks first — on a rollback path they must
        // not fire regardless of whether the rollback itself succeeds.
        self.on_commit.clear();

        match self.inner {
            ContextInner::Pool(_) => Err(DjogiError::Db(DbError::other(
                "DjogiContext::rollback called on a pool-backed context",
            ))),
            ContextInner::Transaction(mut conn) => {
                conn.batch_execute("ROLLBACK").await?;
                Ok(())
            }
        }
    }

    /// Begin a transaction and wrap it in a new `DjogiContext`.
    ///
    /// Only valid on pool-backed contexts — returns an error if called on an
    /// already-transaction-backed context (nested transactions will be
    /// modelled via savepoints in Phase 4 Task 1's `atomic()` wrapper).
    ///
    /// This is a low-level helper used by tests and by the `atomic()`
    /// implementation; production code should reach for `atomic()`.
    pub async fn begin(&self) -> Result<DjogiContext, DjogiError> {
        match &self.inner {
            ContextInner::Pool(pool) => {
                let mut conn = pool.get().await?;
                conn.batch_execute("BEGIN").await?;
                // Share the parent's Sassi handle — the registry is read-only
                // after construction and transaction scope does not change the
                // set of registered pools. `begin()` / `atomic()` are inner
                // contexts that share the outer's cache state.
                //
                // SYNC: the field list below must stay in lockstep with
                // `from_pool` and `from_connection`.
                Ok(DjogiContext::from_connection_with_sassi(
                    conn,
                    Arc::clone(&self.sassi),
                ))
            }
            ContextInner::Transaction(_) => Err(DjogiError::Db(DbError::other(
                "DjogiContext::begin called on a transaction-backed context; \
                 nested transactions require atomic() (Phase 4 Task 1)",
            ))),
        }
    }

    /// Register an async callback to fire after a successful commit.
    ///
    /// # Behavior by context kind
    ///
    /// - **Transaction-backed** context (inside `atomic()` or after
    ///   [`begin`](Self::begin)): the callback is queued and fires in FIFO
    ///   order after the underlying transaction commits. Callback errors
    ///   are logged via `tracing::error!` but do not fail the commit (per
    ///   the spec resolution). Subsequent callbacks still fire even if an
    ///   earlier callback fails.
    /// - **Pool-backed** context (no surrounding transaction): the callback
    ///   is **dropped without queuing** and a `#[track_caller]
    ///   tracing::warn!` event with the grep-able token
    ///   `djogi::on_commit::pool_backed_drop` fires at the caller's source
    ///   location. There is no `commit()` to drain the queue against on a
    ///   pool-backed context, so silently queuing the callback would lose
    ///   work; emitting an audit-warn instead makes the silent-drop
    ///   observable for log scrapers.
    ///
    /// # Recommended fix for pool-backed callers
    ///
    /// Wrap the CRUD chain (or just the `on_commit` registration) in
    /// [`djogi::transaction::atomic`](crate::transaction::atomic):
    ///
    /// ```ignore
    /// djogi::transaction::atomic(&mut ctx, |ctx| Box::pin(async move {
    ///     Foo::create(ctx, foo).await?;
    ///     ctx.on_commit(move || async move { /* fires after commit */ Ok(()) });
    ///     Ok::<_, djogi::DjogiError>(())
    /// })).await?;
    /// ```
    ///
    /// `atomic()` performs the canonical Phase 8 §D3 drain sequence
    /// (`before → DB → outbox → after → on_commit`) so the callback runs
    /// exactly once after the surrounding transaction commits.
    ///
    /// # Audit-warn semantics
    ///
    /// The warn is **single-shot per `on_commit` call** — it does not
    /// amplify per row or per drain step. The `#[track_caller]` attribute
    /// surfaces the adopter's call site rather than this method, so log
    /// scrapers can pinpoint which application code needs the `atomic()`
    /// wrap. The grep-able token `djogi::on_commit::pool_backed_drop` is
    /// stable; treat it as part of the audit invariant.
    #[track_caller]
    pub fn on_commit<F, Fut>(&mut self, callback: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), DjogiError>> + Send + 'static,
    {
        match &self.inner {
            ContextInner::Pool(_) => {
                // Pool-backed context: no transaction commit will ever run
                // to drain queued callbacks. Drop the callback and emit a
                // grep-able warn at the caller's site so silent failures
                // are observable to log scrapers. Mirrors the Phase 5
                // `_insecurely` audit-warn pattern.
                tracing::warn!(
                    caller = %std::panic::Location::caller(),
                    "djogi::on_commit::pool_backed_drop: on_commit callback dropped on a \
                     pool-backed DjogiContext (no transaction commit will run to drain it); \
                     wrap the call in djogi::transaction::atomic(..) so the on_commit drain fires",
                );
                drop(callback);
            }
            ContextInner::Transaction(_) => {
                let boxed: OnCommitCallback = Box::new(move || Box::pin(callback()));
                self.on_commit.push(boxed);
            }
        }
    }

    /// Length of the on-commit callback queue. Used by `transaction.rs`
    /// to snapshot the queue before entering a nested `atomic()` scope
    /// so inner-registered callbacks can be dropped on rollback.
    pub(crate) fn on_commit_len(&self) -> usize {
        self.on_commit.len()
    }

    /// Truncate the on-commit callback queue to `new_len`. Used by
    /// `transaction.rs` to discard callbacks registered inside a
    /// nested `atomic()` scope that rolled back.
    pub(crate) fn on_commit_truncate(&mut self, new_len: usize) {
        self.on_commit.truncate(new_len);
    }
}

impl DjogiContext {
    /// Idempotently create a Postgres enum type.
    ///
    /// Postgres does not support `CREATE TYPE … IF NOT EXISTS` for enum
    /// types, so the standard idempotent form is a `DO $$ BEGIN CREATE
    /// TYPE … EXCEPTION WHEN duplicate_object THEN NULL; END $$` block.
    /// This helper builds and runs that block for you.
    ///
    /// # Validation
    ///
    /// `name` is validated as a plain Postgres identifier (ASCII letter
    /// or underscore first, alphanumerics or underscores after, ≤ 63
    /// bytes, never a reserved Postgres keyword). Each variant string
    /// may carry any text (single quotes are escaped per Postgres rules)
    /// but must be ≤ 63 bytes. Empty `variants` is rejected — Postgres
    /// requires at least one label.
    ///
    /// # Idempotency
    ///
    /// Re-invoking `ensure_enum_type` with the same name and variants
    /// is a no-op (the inner `CREATE TYPE` raises `42710 duplicate_object`,
    /// the `EXCEPTION` arm catches it, the outer block returns
    /// successfully). Re-invoking with the **same name but different
    /// variants** is also a no-op — Postgres does not compare variant
    /// lists; the `42710` arm fires regardless. Variant evolution
    /// (add / drop / rename) belongs to a real migration, not this
    /// helper.
    ///
    /// # Errors
    ///
    /// Returns `DjogiError::Db` for invalid identifiers (framework-side
    /// validation) or any underlying execution failure (network,
    /// permissions, transaction-state mismatch, etc.).
    ///
    /// # Example
    ///
    /// ```ignore
    /// ctx.ensure_enum_type("color", &["red", "green", "blue"]).await?;
    /// ```
    pub async fn ensure_enum_type(
        &mut self,
        name: &str,
        variants: &[&str],
    ) -> Result<(), DjogiError> {
        validate_runtime_plain_ident(name, "enum type name")?;
        if variants.is_empty() {
            return Err(DjogiError::Db(crate::error::DbError::other(
                "ensure_enum_type requires at least one variant; Postgres rejects \
                 `CREATE TYPE … AS ENUM ()` with no labels",
            )));
        }
        for v in variants {
            if v.len() > 63 {
                return Err(DjogiError::Db(crate::error::DbError::other(format!(
                    "enum variant {v:?} is {len} bytes; Postgres enum labels are \
                     limited to 63 bytes (NAMEDATALEN - 1)",
                    len = v.len(),
                ))));
            }
            // Reject `$` in variants. The wrapping DO block uses
            // dollar-quoting (see `TAG` below); Postgres scans the
            // dollar-quoted body for the next matching tag regardless
            // of single-quote context inside it. A variant containing
            // the tag string would close the body early — a SQL-
            // injection surface for adopters who accept variant names
            // from external input. The simplest defence is to reject
            // `$` entirely; real-world enum labels (`'red'`,
            // `'pending'`, `'approved'`, etc.) never need it. Adopters
            // who genuinely need `$` in a label can call
            // `ctx.raw_ddl(...)` directly with their own escape
            // discipline.
            if v.as_bytes().contains(&b'$') {
                return Err(DjogiError::Db(crate::error::DbError::other(format!(
                    "enum variant {v:?} contains `$`; ensure_enum_type rejects \
                     `$` in variants because the wrapping DO block uses \
                     dollar-quoted strings whose terminator could be smuggled \
                     by hostile input. If you genuinely need `$` in a label, \
                     write the DDL with `ctx.raw_ddl(...)` directly.",
                ))));
            }
        }
        let mut sql =
            String::with_capacity(96 + variants.iter().map(|v| v.len() + 4).sum::<usize>());
        // Use a uniquely-named dollar-quote tag rather than the bare
        // `$$` form. Combined with the per-variant `$` rejection above,
        // this gives belt-and-suspenders protection: even if a future
        // change accidentally relaxes the variant validator, the tag
        // name is specific enough that an attacker would have to
        // smuggle it exactly. Two layers, two failure modes blocked.
        const TAG: &str = "$djogi_ensure_enum$";
        sql.push_str("DO ");
        sql.push_str(TAG);
        sql.push_str(" BEGIN CREATE TYPE ");
        sql.push_str(name);
        sql.push_str(" AS ENUM (");
        for (i, v) in variants.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push('\'');
            for c in v.chars() {
                if c == '\'' {
                    sql.push('\'');
                }
                sql.push(c);
            }
            sql.push('\'');
        }
        sql.push_str("); EXCEPTION WHEN duplicate_object THEN NULL; END ");
        sql.push_str(TAG);
        self.batch_execute(&sql).await
    }

    /// Set the active tenant ID for this context by issuing
    /// `set_config('app.tenant_id', $1, true)`.
    ///
    /// This activates Row Level Security policies that use
    /// `current_setting('app.tenant_id')` as the tenant discriminator. After
    /// this call returns `Ok`, `self.tenant_set` is `true` so caller code can
    /// assert the precondition before performing tenant-scoped queries.
    ///
    /// # Important: use inside `atomic()`
    ///
    /// `set_config(…, true)` is the transactional form of `SET LOCAL` — the
    /// GUC is scoped to the **current transaction** and resets when the
    /// transaction commits or rolls back. On a pool-backed context without an
    /// open transaction, the GUC change is immediately effective but only
    /// lasts for the duration of the single statement, then the connection
    /// returns to the pool with the GUC cleared.
    ///
    /// The correct production pattern is:
    ///
    /// ```ignore
    /// let mut ctx = DjogiContext::from_pool(pool.clone());
    /// atomic(&mut ctx, |ctx| Box::pin(async move {
    ///     ctx.set_tenant("42").await?;
    ///     // All queries inside atomic() now see the tenant-scoped rows.
    ///     let posts = TenantPost::objects().fetch_all(ctx).await?;
    ///     Ok(posts)
    /// })).await?
    /// ```
    ///
    /// # Arguments
    ///
    /// * `tenant_id` — The tenant identifier as a string. For `BIGINT` tenant
    ///   keys the RLS policy casts the GUC value to the appropriate type
    ///   (e.g. `current_setting('app.tenant_id', true)::bigint`), so any
    ///   string that Postgres can cast to the column type is valid here.
    pub async fn set_tenant(&mut self, tenant_id: &str) -> Result<(), DjogiError> {
        // `set_config(name, value, is_local)` is the function-form of
        // `SET LOCAL` when `is_local = true`. Using a parameterised query via
        // `$1` lets the driver handle quoting; embedding the value directly
        // into SQL would require manual escaping and violates the no-interpolation
        // policy on user-supplied data.
        self.execute(
            "SELECT set_config('app.tenant_id', $1, true)",
            &[&tenant_id],
        )
        .await?;
        self.tenant_set = true;
        self.applied_tenant_id = Some(tenant_id.to_owned());
        Ok(())
    }

    /// Clear the `app.tenant_id` GUC for the current transaction by
    /// issuing `SELECT set_config('app.tenant_id', '', true)`, then reset
    /// the in-memory `applied_tenant_id` / `tenant_set` trackers.
    ///
    /// Used by the auto-tenant integration (Phase 5.5 Task 10) when auth
    /// is swapped from an identity carrying `tenant_id = Some(...)` to one
    /// that has no tenant scope (`tenant_id = None`): without this reset,
    /// the earlier `SET LOCAL` would remain in effect for the rest of the
    /// transaction and subsequent tenant-keyed queries would silently
    /// leak across tenants. Issues an empty-string value which the `=`
    /// predicate in `current_setting('app.tenant_id', true)`-based RLS
    /// policies treats as unmatched, and which fails the `::bigint` cast
    /// on numeric tenant_key columns — the net effect is "no rows
    /// visible" rather than "rows under the old tenant visible".
    pub async fn clear_tenant(&mut self) -> Result<(), DjogiError> {
        self.execute("SELECT set_config('app.tenant_id', $1, true)", &[&""])
            .await?;
        self.tenant_set = false;
        self.applied_tenant_id = None;
        Ok(())
    }

    /// Return the tenant id currently applied to this context via
    /// [`Self::set_tenant`] / [`Self::ensure_tenant_set`], or `None` if
    /// no `SET LOCAL app.tenant_id = ...` has been issued in the current
    /// transaction.
    ///
    /// Primarily useful for introspection and regression tests. Production
    /// code typically relies on the auto-wiring (Phase 5.5 Task 10) to keep
    /// `app.tenant_id` aligned with `ctx.auth().tenant_id` automatically.
    pub fn applied_tenant_id(&self) -> Option<&str> {
        self.applied_tenant_id.as_deref()
    }

    /// Switch the Postgres session role for the remainder of the current
    /// transaction by issuing `SET LOCAL ROLE "<role>"`.
    ///
    /// This is the security-overlay primitive the row-level-security
    /// helpers compose on top of — it lets a transaction temporarily
    /// drop into a less-privileged role (e.g. `app_readonly`) so a
    /// downstream subquery runs against narrower RLS predicates than
    /// the connecting user otherwise carries.
    ///
    /// # Validation
    ///
    /// Postgres refuses to bind parameters (`$1`) on `SET LOCAL ROLE`,
    /// so the role name is interpolated directly into the SQL. The
    /// byte-level validator below is the substitute defence: the input
    /// must match the Postgres unquoted-identifier grammar — an ASCII
    /// letter or underscore followed by ASCII alphanumerics or
    /// underscores, up to 63 bytes (`NAMEDATALEN - 1`). No embedded
    /// quotes, no control characters, no non-ASCII bytes. The
    /// validation runs via byte-level primitives (`u8::is_ascii_*`,
    /// equality on individual bytes); there is no Rust regex engine
    /// involved, in line with the framework-wide ban on regex
    /// dependencies.
    ///
    /// Reserved-keyword rejection is intentionally **not** applied to
    /// role names. Postgres allows reserved words as role names when
    /// double-quoted (which is exactly how this method emits them), so
    /// rejecting `select` or `where` as a role name would be stricter
    /// than the server itself. The validator delegates to
    /// [`crate::ident::check_plain_ident`] with `check_reserved =
    /// false`, mirroring the runtime-helper convention established by
    /// [`Self::ensure_enum_type`].
    ///
    /// # Quoting and injection surface
    ///
    /// After validation succeeds, the role is wrapped in double quotes
    /// and emitted as `SET LOCAL ROLE "<role>"`. Every byte that could
    /// escape the double-quoted form (`"`, `\`, control characters,
    /// any non-ASCII or non-identifier byte) is rejected by the
    /// validator before the SQL is built — the validator and the
    /// quoting are designed in lockstep, neither layer can stand on
    /// its own.
    ///
    /// # Scope
    ///
    /// `SET LOCAL ROLE` binds the role change to the surrounding
    /// transaction; it reverts at COMMIT or ROLLBACK. Calling this on
    /// a pool-backed `DjogiContext` would either be a no-op (the
    /// connection returns to the pool with the role cleared) or —
    /// worse, in the absence of `LOCAL` — leak the role onto the
    /// pooled connection where the next checkout-victim inherits it.
    /// Both outcomes are programming errors, so this method refuses to
    /// run on a pool-backed context and returns
    /// [`DjogiError::SetRoleOutsideTransaction`] instead. Wrap the
    /// call in [`crate::transaction::atomic`] to get a transaction
    /// scope.
    ///
    /// # Errors
    ///
    /// - [`DjogiError::SetRoleOutsideTransaction`] if `self` is
    ///   pool-backed.
    /// - [`DjogiError::InvalidRoleName`] (carrying the offending
    ///   string) if the role fails byte-level validation.
    /// - [`DjogiError::Db`] for any underlying `tokio_postgres` error
    ///   reported by the server (e.g. role does not exist, current
    ///   user is not a member of the target role).
    ///
    /// # Example
    ///
    /// ```ignore
    /// djogi::transaction::atomic(&mut ctx, |ctx| Box::pin(async move {
    ///     ctx.set_role("app_readonly").await?;
    ///     // Subsequent statements run under the app_readonly role
    ///     // until this atomic() exits.
    ///     let posts = Post::objects().fetch_all(ctx).await?;
    ///     Ok(posts)
    /// })).await?;
    /// ```
    pub async fn set_role(&mut self, role: &str) -> Result<(), DjogiError> {
        // Step 1 — refuse pool-backed contexts. `SET LOCAL ROLE` is
        // bound to the surrounding transaction; without one, the role
        // change either evaporates after the single statement or
        // leaks onto the pooled connection. Both outcomes are misuse,
        // so we surface a typed terminal error before touching SQL.
        // Mirrors the discriminant pattern in
        // `_execute_savepoint_unchecked`.
        match &self.inner {
            ContextInner::Pool(_) => return Err(DjogiError::SetRoleOutsideTransaction),
            ContextInner::Transaction(_) => {}
        }
        // Step 2 — byte-level identifier validation. Delegates to the
        // shared `validate_role_name` free function (defined below),
        // which routes through `check_plain_ident` to enforce
        // non-empty, ≤ 63 bytes, ASCII letter or underscore first,
        // ASCII alphanumerics or underscores after. Reserved keywords
        // are *not* rejected (Postgres permits reserved words as role
        // names when double-quoted, which is exactly how the SQL
        // below emits them). The free-function form lets tests
        // exercise the same gate the production path uses — no
        // mirrored helper, no drift risk.
        validate_role_name(role)?;
        // Step 3 — emit `SET LOCAL ROLE "<role>"`. Postgres rejects
        // parameter binding on `SET LOCAL ROLE`, so the validated
        // identifier is interpolated directly. Every byte that could
        // escape the double-quoted form (`"`, `\`, controls, non-
        // ASCII) was rejected in step 2 — the validator is the
        // substitute defence for the missing parameter slot.
        let sql = format!("SET LOCAL ROLE \"{role}\"");
        self.execute(&sql, &[]).await?;
        Ok(())
    }
}

/// Validate a Postgres role name for `SET LOCAL ROLE`.
///
/// Rules — ASCII letter or underscore as the first byte; ASCII
/// alphanumerics or underscores thereafter; total length 1..=63 bytes;
/// no embedded quotes or control characters. Validation runs via
/// byte-level primitives only — no regex engine.
///
/// Used by `DjogiContext::set_role` (production) AND by the
/// in-module tests that pin the byte-level rejection table. Sharing
/// one function prevents drift between the production security gate
/// and the test fixtures that verify it.
///
/// Routes through `check_plain_ident` (NOT
/// `check_user_supplied_ident`) on purpose: Postgres role names live
/// in the role catalog, a separate namespace from the relations,
/// columns, and CTE aliases djogi reserves under `__djogi_*`. A role
/// named `__djogi_admin` cannot collide with any framework-internal
/// identifier and the SQL emission double-quotes it
/// (`SET LOCAL ROLE "<role>"`) for an extra layer of escape safety.
/// Applying the reservation here would over-restrict adopters
/// without any defensive benefit.
fn validate_role_name(role: &str) -> Result<(), DjogiError> {
    if crate::ident::check_plain_ident(role, false).is_err() {
        return Err(DjogiError::InvalidRoleName(role.to_string()));
    }
    Ok(())
}

/// Runtime plain-identifier validator for user-facing helpers like
/// [`DjogiContext::ensure_enum_type`]. Mirrors the macro-time validator
/// in `djogi-macros::ident::check_one` (which panics) and the runtime
/// const-side validator in `djogi::ident::assert_plain_ident` (which
/// also panics) — a third entry point is needed because user-supplied
/// runtime strings should error rather than panic.
///
/// Rules: non-empty, ≤ 63 bytes, ASCII letter or underscore first,
/// alphanumerics or underscores after the first byte. Reserved-keyword
/// rejection is intentionally **not** applied here because some runtime
/// helpers (e.g. enum-type names that match a reserved word like
/// `interval`) are fine when wrapped in `DO`-block context. Keeping
/// the keyword check minimal matches Postgres's actual identifier-
/// acceptance gradient.
///
/// Routes through [`crate::ident::check_user_supplied_ident`] so the
/// framework-reserved `__djogi_` prefix is rejected here too — runtime
/// type names flow into framework-emitted SQL the same way window
/// aliases and outbox table names do, and the reservation must be
/// uniform across every adopter-facing entry point.
fn validate_runtime_plain_ident(value: &str, role: &str) -> Result<(), DjogiError> {
    use crate::ident::IdentError;
    crate::ident::check_user_supplied_ident(value, false).map_err(|e| {
        let msg = match e {
            IdentError::Empty => format!("{role} cannot be empty"),
            IdentError::TooLong { len } => format!(
                "{role} {value:?} is {len} bytes; Postgres limits identifiers to 63 bytes (NAMEDATALEN - 1)"
            ),
            IdentError::BadFirst { .. } => {
                format!("{role} {value:?} must start with an ASCII letter or underscore")
            }
            IdentError::BadByte { .. } => format!(
                "{role} {value:?} contains a character that is not a valid \
                 unquoted Postgres identifier byte; only ASCII alphanumerics \
                 and underscores are permitted after the first character"
            ),
            IdentError::Reserved => unreachable!("check_user_supplied_ident(reserved=false) cannot return Reserved"),
            IdentError::ReservedDjogiPrefix => format!(
                "{role} {value:?} starts with the framework-reserved `__djogi_` prefix; \
                 this namespace is used for djogi-internal identifiers — choose a \
                 different name"
            ),
        };
        DjogiError::Db(crate::error::DbError::other(msg))
    })
}

/// Drain a batch of on-commit callbacks panic-safely.
///
/// Wraps each callback future in `AssertUnwindSafe(..).catch_unwind()`
/// so a panicking callback is logged via `tracing::error!` without
/// aborting the drain loop. Callback `Err` returns are likewise logged
/// and ignored — per the spec a callback failure must not fail the
/// commit, and every subsequent callback still fires.
pub(crate) async fn drain_on_commit(callbacks: Vec<OnCommitCallback>) {
    for cb in callbacks {
        let result = AssertUnwindSafe(cb()).catch_unwind().await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(
                error = ?e,
                "on_commit callback returned Err; continuing",
            ),
            Err(panic_payload) => {
                let msg = panic_payload
                    .downcast_ref::<&'static str>()
                    .copied()
                    .or_else(|| panic_payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("<non-string panic payload>");
                tracing::error!(
                    panic = %msg,
                    "on_commit callback panicked; continuing",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn savepoint_depth_starts_at_zero_and_increments() {
        // NOTE: Can't create a DjogiContext without a real pool in a blocking context.
        // The pool tests will be covered in integration tests with #[tokio::test].
        // For now, we document the constraint in CLAUDE.md notes and verify
        // compilation + clippy + fmt at the unit test level.
    }

    fn err_msg(role: &str, value: &str) -> String {
        match validate_runtime_plain_ident(value, role) {
            Err(e) => e.to_string(),
            Ok(()) => panic!("expected validate_runtime_plain_ident({role:?}, {value:?}) to error"),
        }
    }

    #[test]
    fn validate_runtime_plain_ident_accepts_letter_first() {
        assert!(validate_runtime_plain_ident("color", "enum type name").is_ok());
        assert!(validate_runtime_plain_ident("_priv", "enum type name").is_ok());
        assert!(validate_runtime_plain_ident("a1_b2_c3", "enum type name").is_ok());
    }

    #[test]
    fn validate_runtime_plain_ident_rejects_empty() {
        let m = err_msg("enum type name", "");
        assert!(m.contains("cannot be empty"), "got: {m}");
    }

    #[test]
    fn validate_runtime_plain_ident_rejects_leading_digit() {
        let m = err_msg("enum type name", "9color");
        assert!(m.contains("ASCII letter or underscore"), "got: {m}");
    }

    #[test]
    fn validate_runtime_plain_ident_rejects_non_ascii() {
        let m = err_msg("enum type name", "color-hyphen");
        assert!(
            m.contains("not a valid unquoted Postgres identifier byte"),
            "got: {m}",
        );
        let m = err_msg("enum type name", "café");
        assert!(
            m.contains("not a valid unquoted Postgres identifier byte"),
            "got: {m}",
        );
    }

    #[test]
    fn validate_runtime_plain_ident_rejects_oversized() {
        let big = "a".repeat(64);
        let m = err_msg("enum type name", &big);
        assert!(m.contains("63 bytes (NAMEDATALEN - 1)"), "got: {m}",);
    }

    #[test]
    fn validate_runtime_plain_ident_accepts_exact_max_length() {
        let exact = "a".repeat(63);
        assert!(validate_runtime_plain_ident(&exact, "enum type name").is_ok());
    }

    #[test]
    fn validate_runtime_plain_ident_rejects_djogi_reserved_prefix() {
        // Issue #82 — `ensure_enum_type` accepts an adopter-supplied
        // schema identifier. The public contract reserves `__djogi_*`
        // across user-facing SQL identifier surfaces so future
        // framework objects can use that namespace without per-surface
        // exceptions.
        let err = validate_runtime_plain_ident("__djogi_status", "enum type name")
            .expect_err("must reject __djogi_ prefix");
        let msg = err.to_string();
        assert!(msg.contains("`__djogi_` prefix"), "got: {msg}");
        // Bare prefix is also rejected.
        assert!(validate_runtime_plain_ident("__djogi_", "enum type name").is_err());
        // Lookalikes remain accepted.
        assert!(validate_runtime_plain_ident("djogi_status", "enum type name").is_ok());
        assert!(validate_runtime_plain_ident("_djogi_status", "enum type name").is_ok());
    }

    #[test]
    fn validate_role_name_keeps_djogi_prefix_legal() {
        // Postgres role names live in a separate namespace from the
        // relations, columns, and CTE aliases djogi reserves under
        // `__djogi_*`. The SQL emission also double-quotes the role,
        // so a `__djogi_admin` role cannot smuggle injection through
        // any djogi surface. Pin the carve-out so a future "make all
        // ident validators uniform" refactor doesn't accidentally
        // tighten this surface.
        assert!(super::validate_role_name("__djogi_admin").is_ok());
        assert!(super::validate_role_name("__djogi_").is_ok());
    }

    // -------------------------------------------------------------------------
    // Phase 8ε T9.2 — set_role byte-level validation gate.
    //
    // Pin the validator's byte-level reject/accept table directly via
    // `super::validate_role_name` (the same free function `set_role`
    // uses in production). No mirrored helper, no drift risk: every
    // byte the production gate accepts or rejects is exactly what
    // these tests assert against. The DB-touch arms (pool-vs-tx
    // discriminant, end-to-end SET LOCAL ROLE round-trip) live in
    // `tests/integration/phase8_set_role_transaction_scoped.rs` —
    // exercising those here would just shadow the integration suite.
    // -------------------------------------------------------------------------

    #[test]
    fn set_role_rejects_quote_injection() {
        // The classic injection shape — a quote-and-trail payload that
        // would close the surrounding `"..."` quoting in the emitted
        // SQL and append a destructive statement. The byte-level
        // validator must reject it at the first non-identifier byte
        // (the embedded `"`), and the typed error must carry the
        // offending input verbatim so logs identify what was blocked.
        let attack = "readonly\"; DROP TABLE users--";
        match super::validate_role_name(attack) {
            Err(DjogiError::InvalidRoleName(s)) => assert_eq!(s, attack),
            other => panic!("expected InvalidRoleName({attack:?}), got {other:?}"),
        }
    }

    #[test]
    fn set_role_rejects_empty_string() {
        // Empty string — `check_plain_ident` returns `IdentError::Empty`,
        // which the gate maps to `InvalidRoleName("")`. Pinning this
        // case explicitly because an empty identifier is the canonical
        // boundary defect: `SET LOCAL ROLE ""` is malformed SQL, but
        // a sloppy validator that only checked "first byte is alpha"
        // would index out of bounds before reaching the alpha check.
        match super::validate_role_name("") {
            Err(DjogiError::InvalidRoleName(s)) => assert_eq!(s, ""),
            other => panic!("expected InvalidRoleName(\"\"), got {other:?}"),
        }
    }

    #[test]
    fn set_role_rejects_overlong_name() {
        // 64-byte role name — exactly one byte over Postgres's 63-byte
        // unquoted identifier limit (`NAMEDATALEN - 1`). The server
        // would silently truncate at 63, which means two Rust-distinct
        // role names could collide after truncation. Reject up front
        // so Rust- and SQL-level identity contracts stay aligned.
        let overlong = "a".repeat(64);
        assert_eq!(overlong.len(), 64);
        match super::validate_role_name(&overlong) {
            Err(DjogiError::InvalidRoleName(s)) => assert_eq!(s, overlong),
            other => panic!("expected InvalidRoleName(<64-byte>), got {other:?}"),
        }
    }

    #[test]
    fn set_role_accepts_exactly_63_byte_name() {
        // Boundary case — 63 bytes is exactly Postgres's
        // `NAMEDATALEN - 1` ceiling for unquoted identifiers. The
        // overlong test above pins the reject side at 64; this pins
        // the accept side at 63 so an off-by-one regression in
        // `check_plain_ident` (e.g. `>` flipped to `>=`) trips a
        // unit test rather than escaping to a live-DB integration
        // run. Closes the boundary gap Codex INFO-3 flagged.
        let exact = "a".repeat(63);
        assert_eq!(exact.len(), 63);
        assert!(super::validate_role_name(&exact).is_ok());
    }

    #[test]
    fn set_role_accepts_valid_identifier() {
        // Positive case — a well-formed role name with the full set of
        // allowed bytes (lowercase letters, digits, underscores, an
        // underscore-led prefix segment). The validation gate must
        // pass; the end-to-end SET LOCAL ROLE round-trip is covered
        // by `tests/integration/phase8_set_role_transaction_scoped.rs`.
        assert!(super::validate_role_name("app_readonly_role").is_ok());
        assert!(super::validate_role_name("_internal").is_ok());
        assert!(super::validate_role_name("role1").is_ok());
    }
}
