//! `atomic(...)` — the canonical transaction scope + retry helper.
//!
//! # `atomic()` at a glance
//!
//! `atomic(&mut ctx, |tx| Box::pin(async move { ... }))` is the preferred
//! outermost entry point when the caller already has a pool-backed
//! [`DjogiContext`](crate::DjogiContext): it acquires a connection from the
//! context's pool, issues `BEGIN`, wraps the connection in a transaction
//! context that shares the parent context's `Arc<Sassi>`, runs the closure,
//! commits on `Ok`, rolls back on `Err`, and drains the on-commit callback
//! queue after a successful commit. `atomic(&pool, |tx| ...)` remains the
//! compatibility shortcut when no parent context exists; it constructs a fresh
//! top-level context for that transaction. Nested calls —
//! `atomic(&mut *outer, |inner| Box::pin(async move { ... }))` — push a
//! Postgres savepoint rather than opening a new transaction: the inner scope
//! rolls back to / releases the savepoint on `Err`/`Ok` respectively, and on
//! success promotes its on-commit callbacks to the outer context so they drain
//! once at the outermost commit.
//!
//! # Isolation level
//!
//! [`atomic_with`] is the sibling helper that opens the outermost transaction
//! at an explicit Postgres isolation level via `BEGIN ISOLATION LEVEL <level>`.
//! See the [`IsolationLevel`] enum docs for the variant matrix. The nested
//! savepoint path explicitly rejects an isolation-level argument because
//! Postgres pins the isolation level for the entire transaction at the outer
//! `BEGIN` — `SAVEPOINT` does not open a sub-transaction with its own
//! isolation knob.
//!
//! # Panic semantics
//!
//! Closure panics never leak an uncommitted transaction. The closure
//! future is wrapped in
//! [`AssertUnwindSafe`](std::panic::AssertUnwindSafe) and polled through
//! [`FutureExt::catch_unwind`](futures::FutureExt::catch_unwind); on
//! the panic branch we issue the appropriate rollback (outer
//! `ROLLBACK` or `ROLLBACK TO SAVEPOINT`) **before** the
//! panic resumes. `AssertUnwindSafe` is the conventional escape hatch
//! for async boundaries — user-supplied closures rarely implement
//! `UnwindSafe`, and the context state touched inside the closure is
//! owned for the closure's lifetime alone.
//!
//! # Retry helper
//!
//! [`retry_on_conflict`] composes with `atomic()` (and `atomic_with()`)
//! to re-run a closure on serialization / deadlock / `NOWAIT` failures.
//! `IsolationLevel::Serializable` / `IsolationLevel::RepeatableRead` raise
//! SQLSTATE `40001` (serialization failure) on commit-time conflict;
//! [`crate::DjogiError::is_transient`] classifies that as retryable, so the
//! retry loop drives the typed isolation surface without extra wiring. Pure
//! retry — no backoff — per the Phase 4 Task 1 scope.

use crate::context::{ContextInner, DjogiContext};
use crate::pg::pool::DjogiPool;
use crate::{DbError, DjogiError};
use futures::FutureExt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, resume_unwind};
use std::pin::Pin;

/// Boxed future tied to the caller's context-reborrow lifetime.
///
/// Every `atomic()` closure returns one of these. The `'a` lifetime
/// ties the future's body to the `&'a mut DjogiContext` the closure
/// receives, so the closure can freely `.await` framework calls that
/// also borrow from the context. The `Pin<Box<..>>` erasure is what
/// lets the outer `atomic()` signature use a `for<'a>` higher-ranked
/// bound without falling into the "async closure implementation not
/// general enough" inference hole that bare `AsyncFnOnce` hits today.
pub type AtomicFuture<'a, R> = Pin<Box<dyn Future<Output = Result<R, DjogiError>> + Send + 'a>>;

// ---------------------------------------------------------------------------
// Isolation level — typed surface for `BEGIN ISOLATION LEVEL <level>`.
// ---------------------------------------------------------------------------

/// Postgres transaction isolation level.
///
/// Maps 1:1 to the SQL standard's three isolation levels Postgres
/// actually distinguishes. (Postgres also accepts `READ UNCOMMITTED`
/// but aliases it to `READ COMMITTED` — it provides no weaker
/// guarantees than `READ COMMITTED` on Postgres so the enum does not
/// expose that variant.)
///
/// Used by [`atomic_with`] to open the outermost transaction at an
/// explicit isolation level via `BEGIN ISOLATION LEVEL <level>`. The
/// level applies to the entire transaction — once the outer `BEGIN`
/// fixes the isolation, Postgres pins it for the duration; `SAVEPOINT`
/// does not open a sub-transaction with its own isolation knob, so
/// nested `atomic_with` calls are rejected (see [`atomic_with`] docs).
///
/// # Variants
///
/// - [`IsolationLevel::ReadCommitted`] — Postgres' session default. A
///   statement sees only data committed before the statement begins
///   (snapshot of the moment the statement starts). Different
///   statements in one transaction can observe different commits. The
///   weakest isolation Postgres provides; widest concurrency.
///
/// - [`IsolationLevel::RepeatableRead`] — every statement in the
///   transaction sees the same snapshot, taken at the moment of the
///   transaction's first non-control statement. Reads are repeatable;
///   concurrent writes that conflict raise SQLSTATE `40001`
///   (serialization_failure) at commit time. [`retry_on_conflict`]
///   classifies that as transient and re-runs the closure.
///
/// - [`IsolationLevel::Serializable`] — strongest. Postgres' SSI
///   (serializable snapshot isolation) monitors read/write dependencies
///   between concurrent transactions and aborts one with `40001` if
///   their interleaving could not be reproduced by some serial
///   execution. Use for invariants that span multiple rows or tables
///   (e.g. "no two events can overlap in the same room").
///
/// # Retry composition
///
/// Both `RepeatableRead` and `Serializable` can raise serialization
/// failure on commit. [`retry_on_conflict`] composes naturally — wrap
/// the `atomic_with` call in `retry_on_conflict` to re-run on `40001`:
///
/// ```ignore
/// retry_on_conflict(&mut ctx, 3, |ctx| async move {
///     atomic_with(IsolationLevel::Serializable, ctx, |tx| Box::pin(async move {
///         // ... reads + writes ...
///         Ok::<_, DjogiError>(())
///     })).await
/// }).await?;
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IsolationLevel {
    /// `READ COMMITTED` — Postgres' session default. See enum docs.
    ReadCommitted,
    /// `REPEATABLE READ` — snapshot fixed at first non-control
    /// statement; serialization-failure on commit conflict. See enum
    /// docs.
    RepeatableRead,
    /// `SERIALIZABLE` — Postgres' strongest isolation via SSI.
    /// Aborts conflicting transactions with `40001`. See enum docs.
    Serializable,
}

impl IsolationLevel {
    /// SQL keyword for this isolation level, matching Postgres' SQL
    /// grammar exactly. Used by [`atomic_with`] to compose the
    /// `BEGIN ISOLATION LEVEL <keyword>` statement.
    ///
    /// The return is `&'static str` — all three keywords are
    /// compile-time literals, no allocation.
    pub const fn as_sql_keyword(self) -> &'static str {
        match self {
            IsolationLevel::ReadCommitted => "READ COMMITTED",
            IsolationLevel::RepeatableRead => "REPEATABLE READ",
            IsolationLevel::Serializable => "SERIALIZABLE",
        }
    }
}

impl std::fmt::Display for IsolationLevel {
    /// Delegate to [`Self::as_sql_keyword`] so `{level}` in error
    /// messages and traces reads as the SQL keyword.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_sql_keyword())
    }
}

/// Compose the `BEGIN ISOLATION LEVEL <level>` statement for `level`.
///
/// Inlined into both pool-path entry points (pool reference, pool-
/// backed context) so the SQL composition lives in one place. Uses
/// [`IsolationLevel::as_sql_keyword`] for the literal — no user
/// input flows into the SQL.
fn begin_with_isolation_sql(level: IsolationLevel) -> String {
    format!("BEGIN ISOLATION LEVEL {}", level.as_sql_keyword())
}

// ---------------------------------------------------------------------------
// DeferScope — typed surface for SET CONSTRAINTS ALL/<names> DEFERRED/IMMEDIATE.
// ---------------------------------------------------------------------------

/// Scope payload for [`DjogiContext::defer_constraints`] /
/// [`DjogiContext::set_constraints_immediate`].
///
/// Mirrors the Postgres `SET CONSTRAINTS { ALL | <name> [, ...] } { DEFERRED |
/// IMMEDIATE }` grammar. Both helpers are transaction-scoped: outside an
/// open `atomic()` they raise
/// [`DjogiError::ConstraintModeOutsideTransaction`].
///
/// # Variants
///
/// - [`DeferScope::All`] — apply the new mode to every deferrable
///   constraint in the current transaction. Emits
///   `SET CONSTRAINTS ALL DEFERRED|IMMEDIATE`. Postgres' standard
///   shape for the cycle-FK insertion pattern.
///
/// - [`DeferScope::Named`] — apply the new mode to specific named
///   constraints. The framework validates each name against the
///   model-descriptor inventory ([`crate::DeferrabilitySpec`])
///   before emitting any SQL:
///   - Unknown name → [`DjogiError::UnknownConstraintName`].
///   - Name found but `deferrable = false` →
///     [`DjogiError::ConstraintNotDeferrable`].
///   - All names valid + deferrable → emit
///     `SET CONSTRAINTS "name1", "name2", ... DEFERRED|IMMEDIATE`.
///
///   The constraint naming convention is the conventional
///   `<table>_<column>_fkey` (truncated to Postgres' 63-byte
///   identifier limit when necessary) — see
///   `djogi/src/migrate/sql.rs::fk_constraint_name` for the canonical
///   composition.
///
/// # Slice form for `Named`
///
/// `Named(&'static [&'static str])` accepts a slice of static names so
/// the typical adopter call site is allocation-free:
///
/// ```ignore
/// ctx.defer_constraints(DeferScope::Named(&["posts_author_id_fkey"])).await?;
/// ```
///
/// `'static` keeps the API simple — constraint names in adopter code
/// are typically string literals known at compile time. Dynamic name
/// composition (rare) can use `Box::leak` or a `&'static str` interner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeferScope {
    /// Apply the mode change to every deferrable constraint in the
    /// transaction. Emits `SET CONSTRAINTS ALL DEFERRED|IMMEDIATE`.
    All,

    /// Apply the mode change to the named constraints. Each name is
    /// validated against the framework's `DeferrabilitySpec`
    /// inventory before any SQL is emitted.
    Named(&'static [&'static str]),
}

/// Constraint-mode keyword used by [`DjogiContext::defer_constraints`]
/// and [`DjogiContext::set_constraints_immediate`] to drive the
/// underlying `SET CONSTRAINTS ... DEFERRED|IMMEDIATE` SQL.
///
/// Private to the crate — the two public entry points wrap this
/// internally so adopters never name the mode directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConstraintMode {
    Deferred,
    Immediate,
}

impl ConstraintMode {
    /// SQL keyword for this mode, used in `SET CONSTRAINTS ...
    /// <keyword>` emission.
    pub(crate) const fn as_sql_keyword(self) -> &'static str {
        match self {
            ConstraintMode::Deferred => "DEFERRED",
            ConstraintMode::Immediate => "IMMEDIATE",
        }
    }
}

// ---------------------------------------------------------------------------
// Sealed trait — `&DjogiPool` and `&mut DjogiContext` are the only scopes.
// ---------------------------------------------------------------------------

mod sealed {
    pub trait Sealed {}
    impl Sealed for &crate::pg::pool::DjogiPool {}
    impl Sealed for &mut crate::DjogiContext {}
}

/// Entry point for [`atomic()`] / [`atomic_with()`].
///
/// Sealed: the only scopes that can open an `atomic()` block are a
/// pool reference (fresh outermost context) and a mutable [`DjogiContext`]
/// reference. A pool-backed `DjogiContext` opens an outermost transaction that
/// shares the context's `Arc<Sassi>`; a transaction-backed context opens a
/// nested savepoint.
///
/// The trait carries the dispatch logic through an associated
/// [`run_atomic`](IntoAtomicScope::run_atomic) method so `atomic()`
/// itself stays as a thin forwarder over the two impls. Each impl
/// owns its own commit / rollback / callback-promotion semantics.
///
/// The companion [`run_atomic_with`](IntoAtomicScope::run_atomic_with)
/// method threads an explicit [`IsolationLevel`] through to the outer
/// `BEGIN`. The pool-path impls compose `BEGIN ISOLATION LEVEL
/// <level>` for the open; the nested-savepoint impl rejects with
/// [`DjogiError::IsolationLevelOnNestedScope`] because Postgres pins
/// isolation at the outer `BEGIN` — savepoints cannot change it.
#[doc(hidden)]
pub trait IntoAtomicScope: sealed::Sealed {
    /// Run `closure` inside this scope. Implementations handle
    /// outermost transaction open + commit/rollback + callback drain
    /// (pool path) or savepoint push + release/rollback-to + callback
    /// promotion (nested path).
    fn run_atomic<F, R>(
        self,
        closure: F,
    ) -> impl std::future::Future<Output = Result<R, DjogiError>> + Send
    where
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send;

    /// Run `closure` inside this scope, threading an explicit Postgres
    /// isolation level through to the outer `BEGIN`.
    ///
    /// Pool-path impls emit `BEGIN ISOLATION LEVEL <level>` instead of
    /// the plain `BEGIN` of [`run_atomic`]. The nested-savepoint impl
    /// returns [`DjogiError::IsolationLevelOnNestedScope`] because
    /// Postgres pins isolation at the outer `BEGIN` for the entire
    /// transaction; `SAVEPOINT` does not open a sub-transaction with
    /// its own isolation knob.
    fn run_atomic_with<F, R>(
        self,
        level: IsolationLevel,
        closure: F,
    ) -> impl std::future::Future<Output = Result<R, DjogiError>> + Send
    where
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send;
}

// ---------------------------------------------------------------------------
// Pool-backed impl — the outermost entry point.
// ---------------------------------------------------------------------------

impl IntoAtomicScope for &DjogiPool {
    async fn run_atomic<F, R>(self, closure: F) -> Result<R, DjogiError>
    where
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send,
    {
        run_pool_atomic_inner(self, "BEGIN", closure).await
    }

    async fn run_atomic_with<F, R>(self, level: IsolationLevel, closure: F) -> Result<R, DjogiError>
    where
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send,
    {
        let begin_sql = begin_with_isolation_sql(level);
        run_pool_atomic_inner(self, &begin_sql, closure).await
    }
}

/// Common body for the pool-path `atomic` / `atomic_with` entry points.
///
/// `begin_sql` is the verbatim BEGIN statement — either `"BEGIN"` for
/// the default-isolation path or `BEGIN ISOLATION LEVEL <level>` for
/// the explicit-isolation path composed by
/// [`begin_with_isolation_sql`]. Centralising the body keeps the
/// commit / rollback / panic semantics in lockstep across both
/// entry points.
async fn run_pool_atomic_inner<F, R>(
    pool: &DjogiPool,
    begin_sql: &str,
    closure: F,
) -> Result<R, DjogiError>
where
    R: Send + 'static,
    F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send,
{
    // Acquire a connection and begin a transaction.
    let mut conn = pool.get().await?;
    conn.batch_execute(begin_sql).await?;
    let mut ctx = DjogiContext::from_connection(conn);

    // Poll the closure through `catch_unwind` so a panic turns into
    // a caught payload. See the module-level panic-semantics docs.
    let result = AssertUnwindSafe(closure(&mut ctx)).catch_unwind().await;

    match result {
        Ok(Ok(value)) => {
            // Closure succeeded — commit and drain on-commit queue.
            ctx.commit().await?;
            Ok(value)
        }
        Ok(Err(err)) => {
            // Closure returned Err — roll the transaction back and
            // surface the original error.
            if let Err(rb_err) = ctx.rollback().await {
                tracing::error!(
                    error = ?rb_err,
                    "atomic: rollback after closure Err failed; returning closure err",
                );
            }
            Err(err)
        }
        Err(panic_payload) => {
            // Closure panicked — roll back before resuming the
            // panic so the transaction doesn't leak.
            if let Err(rb_err) = ctx.rollback().await {
                tracing::error!(
                    error = ?rb_err,
                    "atomic: rollback after closure panic failed; resuming panic",
                );
            }
            resume_unwind(panic_payload);
        }
    }
}

async fn run_pool_context_atomic<F, R>(
    ctx: &mut DjogiContext,
    begin_sql: &str,
    closure: F,
) -> Result<R, DjogiError>
where
    R: Send + 'static,
    F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send,
{
    let pool = ctx.pool().cloned().ok_or_else(|| {
        DjogiError::Db(DbError::other(
            "atomic(&mut ctx, ...) expected a pool-backed context",
        ))
    })?;

    let mut conn = pool.get().await?;
    conn.batch_execute(begin_sql).await?;

    let mut tx_ctx =
        DjogiContext::from_connection_with_sassi(conn, std::sync::Arc::clone(&ctx.sassi));
    tx_ctx.auth = ctx.auth.clone();
    tx_ctx.tenant_scope_suppressed = ctx.tenant_scope_suppressed;

    let result = AssertUnwindSafe(closure(&mut tx_ctx)).catch_unwind().await;

    match result {
        Ok(Ok(value)) => {
            let auth_after = tx_ctx.auth.clone();
            let tenant_scope_suppressed_after = tx_ctx.tenant_scope_suppressed;
            tx_ctx.commit().await?;

            ctx.auth = auth_after;
            ctx.tenant_scope_suppressed = tenant_scope_suppressed_after;
            clear_pool_context_transaction_trackers(ctx);

            Ok(value)
        }
        Ok(Err(err)) => {
            if let Err(rb_err) = tx_ctx.rollback().await {
                tracing::error!(
                    error = ?rb_err,
                    "atomic: rollback after closure Err failed; returning closure err",
                );
            }
            clear_pool_context_transaction_trackers(ctx);
            Err(err)
        }
        Err(panic_payload) => {
            if let Err(rb_err) = tx_ctx.rollback().await {
                tracing::error!(
                    error = ?rb_err,
                    "atomic: rollback after closure panic failed; resuming panic",
                );
            }
            resume_unwind(panic_payload);
        }
    }
}

fn clear_pool_context_transaction_trackers(ctx: &mut DjogiContext) {
    ctx.tenant_set = false;
    ctx.applied_tenant_id = None;
}

// ---------------------------------------------------------------------------
// Nested-context impl — savepoints + callback promotion.
// ---------------------------------------------------------------------------

impl IntoAtomicScope for &mut DjogiContext {
    async fn run_atomic<F, R>(self, closure: F) -> Result<R, DjogiError>
    where
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send,
    {
        if matches!(self.inner_mut(), ContextInner::Pool(_)) {
            return run_pool_context_atomic(self, "BEGIN", closure).await;
        }

        run_nested_savepoint_atomic(self, closure).await
    }

    async fn run_atomic_with<F, R>(self, level: IsolationLevel, closure: F) -> Result<R, DjogiError>
    where
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send,
    {
        match self.inner_mut() {
            ContextInner::Pool(_) => {
                let begin_sql = begin_with_isolation_sql(level);
                run_pool_context_atomic(self, &begin_sql, closure).await
            }
            // Postgres pins the isolation level at the outer `BEGIN`
            // for the entire transaction. `SAVEPOINT` does not open a
            // sub-transaction with its own isolation knob — issuing
            // `SET TRANSACTION ISOLATION LEVEL` mid-transaction
            // (after the first non-control statement, which by the
            // time control reaches here has already executed) is
            // rejected by Postgres with SQLSTATE `25001`. Reject in
            // the framework before any SQL flies so the caller gets
            // a typed error rather than a deferred SQL-error surprise.
            ContextInner::Transaction(_) => {
                Err(DjogiError::IsolationLevelOnNestedScope { requested: level })
            }
        }
    }
}

async fn run_nested_savepoint_atomic<F, R>(
    ctx: &mut DjogiContext,
    closure: F,
) -> Result<R, DjogiError>
where
    R: Send + 'static,
    F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send,
{
    debug_assert!(
        matches!(ctx.inner_mut(), ContextInner::Transaction(_)),
        "run_nested_savepoint_atomic invoked on a non-transaction inner",
    );

    // Push savepoint. Depth is incremented BEFORE the SQL so
    // `sp_<depth>` numbering starts at 1 for the first nested
    // level. `sp_<n>` is ASCII + underscore — safe unquoted.
    ctx.increment_savepoint_depth();
    let depth = ctx.savepoint_depth();
    let savepoint_name = format!("sp_{depth}");

    let sp_sql = format!("SAVEPOINT {savepoint_name}");
    let push_result = match ctx.inner_mut() {
        ContextInner::Transaction(conn) => conn.batch_execute(&sp_sql).await,
        // Unreachable because of the debug_assert above.
        ContextInner::Pool(_) => unreachable!("debug_assert above rules this out"),
    };
    if let Err(e) = push_result {
        ctx.decrement_savepoint_depth();
        return Err(e);
    }

    // Nested path shares the parent context directly — inner
    // writes land on the same transaction, inner on_commit
    // callbacks land on the parent's queue. Snapshot before
    // entering the closure so we can truncate on rollback.
    let callbacks_before = ctx.on_commit_queue_len();

    // Snapshot auth-related state (auth, applied_tenant_id, tenant_set,
    // tenant_scope_suppressed) so savepoint rollback restores the
    // in-memory trackers to match the post-rollback GUC state.
    // Without this, an inner scope that does set_auth(org_b) and
    // triggers set_tenant("org_b") would leave ctx.applied_tenant_id
    // = Some("org_b") after ROLLBACK TO SAVEPOINT reverted the GUC
    // to the outer value — the next tenant-keyed query in the outer
    // scope would then short-circuit (matching applied_tenant_id)
    // and silently run under the wrong tenant. Phase 5.5 phase-
    // boundary fixup (Codex stop-gate review).
    let auth_snapshot = ctx.snapshot_auth_state();

    let inner_result = AssertUnwindSafe(closure(ctx)).catch_unwind().await;

    match inner_result {
        Ok(Ok(value)) => {
            // Success — RELEASE SAVEPOINT. Inner callbacks stay on
            // the parent queue (promoted). Inner auth-state mutations
            // also stay in effect: the caller's atomic scope is now
            // the parent and the inner's choices (e.g., set_auth) are
            // the continuing context.
            let release_sql = format!("RELEASE SAVEPOINT {savepoint_name}");
            let release_res = match ctx.inner_mut() {
                ContextInner::Transaction(conn) => conn.batch_execute(&release_sql).await,
                ContextInner::Pool(_) => unreachable!(),
            };
            ctx.decrement_savepoint_depth();
            release_res?;
            Ok(value)
        }
        Ok(Err(err)) => {
            // Closure returned Err — ROLLBACK TO SAVEPOINT then
            // RELEASE. Discard inner callbacks by truncating the
            // parent queue back to its pre-closure length, and
            // restore auth state so it matches the reverted GUC.
            ctx.truncate_on_commit_queue(callbacks_before);
            ctx.restore_auth_state(auth_snapshot);
            let rb_sql = format!("ROLLBACK TO SAVEPOINT {savepoint_name}");
            let rel_sql = format!("RELEASE SAVEPOINT {savepoint_name}");
            if let Some(rb_err) = ctx.run_rollback_to_release(&rb_sql, &rel_sql).await {
                tracing::error!(
                    error = ?rb_err,
                    "atomic: ROLLBACK TO SAVEPOINT after closure Err failed; \
                     returning closure err",
                );
            }
            ctx.decrement_savepoint_depth();
            Err(err)
        }
        Err(panic_payload) => {
            // Closure panicked — same rollback-then-resume as the
            // pool impl, scoped to the savepoint. Restore auth state
            // before resuming so the parent scope (if it catches
            // the unwind) sees consistent ctx state.
            ctx.truncate_on_commit_queue(callbacks_before);
            ctx.restore_auth_state(auth_snapshot);
            let rb_sql = format!("ROLLBACK TO SAVEPOINT {savepoint_name}");
            let rel_sql = format!("RELEASE SAVEPOINT {savepoint_name}");
            if let Some(rb_err) = ctx.run_rollback_to_release(&rb_sql, &rel_sql).await {
                tracing::error!(
                    error = ?rb_err,
                    "atomic: ROLLBACK TO SAVEPOINT after closure panic failed; \
                     resuming panic",
                );
            }
            ctx.decrement_savepoint_depth();
            resume_unwind(panic_payload);
        }
    }
}

// ---------------------------------------------------------------------------
// Public API.
// ---------------------------------------------------------------------------

/// Run `closure` inside an atomic transaction scope.
///
/// Two shapes:
///
/// - `atomic(&mut pool_ctx, |ctx| Box::pin(async move { ... }))` —
///   preferred outermost form when the caller already has a pool-backed
///   context. Opens a transaction, shares `pool_ctx`'s Sassi registry,
///   commits on `Ok`, rolls back on `Err`, and drains on-commit callbacks
///   after the commit.
/// - `atomic(&pool, |ctx| Box::pin(async move { ... }))` — compatibility
///   shortcut when no parent context exists. Opens a fresh top-level
///   transaction context.
/// - `atomic(&mut tx_ctx, |ctx| Box::pin(async move { ... }))` — nested.
///   Emits `SAVEPOINT sp_<depth>` on entry; `RELEASE` on `Ok`, `ROLLBACK TO
///   SAVEPOINT` + `RELEASE` on `Err`. On-commit callbacks registered inside
///   a nested scope are promoted to the outer queue on success, discarded on
///   `Err`.
///
/// # Panic semantics
///
/// If the closure panics, `atomic()` rolls back (or rolls back to the
/// savepoint, in the nested case) **before** the panic resumes. The
/// transaction never leaks. See the module-level docs for rationale.
///
/// # Examples
///
/// ```ignore
/// let mut ctx = DjogiContext::from_pool(pool.clone());
///
/// djogi::transaction::atomic(&mut ctx, |ctx| Box::pin(async move {
///     Account::create(ctx, Account { balance: 100, ..Default::default() }).await?;
///     Ok::<_, DjogiError>(())
/// }))
/// .await?;
/// ```
pub async fn atomic<S, F, R>(scope: S, closure: F) -> Result<R, DjogiError>
where
    S: IntoAtomicScope,
    R: Send + 'static,
    F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send,
{
    scope.run_atomic(closure).await
}

/// Run `closure` inside an atomic transaction scope, opening the
/// outermost transaction at the requested Postgres isolation level.
///
/// `atomic_with` is the sibling of [`atomic`] for callers that need an
/// explicit isolation level instead of Postgres' session default
/// (`READ COMMITTED`). The level is threaded into the outer `BEGIN`
/// statement as `BEGIN ISOLATION LEVEL <level>`; everything else
/// (commit/rollback semantics, panic safety, on-commit callback drain)
/// matches [`atomic`] exactly.
///
/// # Scopes
///
/// - `atomic_with(level, &mut pool_ctx, ...)` — preferred outermost
///   shape when the caller already holds a pool-backed
///   [`DjogiContext`]. Opens a transaction at `level`, shares the
///   parent context's Sassi registry, commits on `Ok`, rolls back on
///   `Err`.
/// - `atomic_with(level, &pool, ...)` — compatibility shortcut when
///   no parent context exists. Opens a fresh top-level transaction
///   context at `level`.
/// - `atomic_with(level, &mut tx_ctx, ...)` — **rejected** with
///   [`DjogiError::IsolationLevelOnNestedScope`]. Postgres pins
///   isolation at the outer `BEGIN` for the entire transaction;
///   `SAVEPOINT` does not open a sub-transaction with its own
///   isolation knob. Use [`atomic`] for nested scopes — the nested
///   scope inherits the outermost transaction's isolation level.
///
/// # Retry composition
///
/// Both `RepeatableRead` and `Serializable` can raise SQLSTATE
/// `40001` on commit-time conflict. Wrap the `atomic_with` call in
/// [`retry_on_conflict`] to re-run on `40001`:
///
/// ```ignore
/// use djogi::transaction::{atomic_with, retry_on_conflict, IsolationLevel};
///
/// retry_on_conflict(&mut ctx, 3, |ctx| async move {
///     atomic_with(IsolationLevel::Serializable, ctx, |tx| Box::pin(async move {
///         // ... reads + writes that must observe a serial schedule ...
///         Ok::<_, DjogiError>(())
///     })).await
/// }).await?;
/// ```
///
/// # Panic semantics
///
/// Identical to [`atomic`]: a closure panic triggers `ROLLBACK`
/// before the panic resumes; the transaction never leaks.
///
/// # Errors
///
/// - [`DjogiError::IsolationLevelOnNestedScope`] when invoked on a
///   transaction-backed `DjogiContext` (the nested savepoint scope).
///   Classified as **terminal** — retrying cannot turn a nested
///   savepoint into a fresh outermost transaction.
/// - The underlying `BEGIN ISOLATION LEVEL <level>` SQL error if
///   Postgres refuses the request (`25001` after the first
///   statement, etc.). Classified by Postgres' SQLSTATE.
///
/// # Example
///
/// ```ignore
/// use djogi::DjogiContext;
/// use djogi::transaction::{atomic_with, IsolationLevel};
///
/// let mut ctx = DjogiContext::from_pool(pool.clone());
///
/// atomic_with(IsolationLevel::Serializable, &mut ctx, |ctx| Box::pin(async move {
///     // SSI snapshot is fixed at the first statement. Concurrent
///     // writes that violate a multi-row invariant raise 40001 at
///     // commit time; wrap in retry_on_conflict to retry.
///     let total = Account::objects().sum(ctx, |f| f.balance()).await?;
///     // ... act on `total` ...
///     Ok::<_, DjogiError>(())
/// })).await?;
/// ```
pub async fn atomic_with<S, F, R>(
    level: IsolationLevel,
    scope: S,
    closure: F,
) -> Result<R, DjogiError>
where
    S: IntoAtomicScope,
    R: Send + 'static,
    F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send,
{
    scope.run_atomic_with(level, closure).await
}

/// Re-run `closure` up to `attempts` times on lock / serialization
/// conflicts.
///
/// Classifies errors via [`crate::DjogiError::is_transient`] — SQLSTATEs
/// `40001`, `40P01`, `55P03` are considered retryable. Every other
/// error (constraint violations, not-found, etc.) surfaces on the
/// first call. Pure retry with no backoff today — exponential / jittered
/// backoff is intentionally deferred until the need is measured.
pub async fn retry_on_conflict<F, R>(
    ctx: &mut DjogiContext,
    attempts: u32,
    mut closure: F,
) -> Result<R, DjogiError>
where
    F: AsyncFnMut(&mut DjogiContext) -> Result<R, DjogiError>,
{
    debug_assert!(attempts >= 1, "attempts must be >= 1");
    let mut attempt: u32 = 0;
    loop {
        attempt = attempt.saturating_add(1);
        match closure(ctx).await {
            Ok(value) => return Ok(value),
            Err(e) => {
                let retryable = e.is_transient();
                if retryable && attempt < attempts {
                    tracing::debug!(
                        attempt,
                        attempts,
                        "retry_on_conflict: retryable lock error; retrying",
                    );
                    continue;
                }
                return Err(e);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers bolted onto `DjogiContext` for the nested path.
// ---------------------------------------------------------------------------

impl DjogiContext {
    /// Current length of the on-commit callback queue.
    pub(crate) fn on_commit_queue_len(&self) -> usize {
        self.on_commit_len()
    }

    /// Truncate the on-commit callback queue back to `new_len`.
    pub(crate) fn truncate_on_commit_queue(&mut self, new_len: usize) {
        self.on_commit_truncate(new_len);
    }

    /// Issue `ROLLBACK TO SAVEPOINT` followed by `RELEASE SAVEPOINT`.
    /// Returns `Some(err)` on the first failure, `None` if both
    /// succeeded.
    async fn run_rollback_to_release(&mut self, rb_sql: &str, rel_sql: &str) -> Option<DjogiError> {
        match self.inner_mut() {
            ContextInner::Transaction(conn) => {
                if let Err(e) = conn.batch_execute(rb_sql).await {
                    return Some(e);
                }
                if let Err(e) = conn.batch_execute(rel_sql).await {
                    return Some(e);
                }
                None
            }
            ContextInner::Pool(_) => Some(DjogiError::Db(DbError::other(
                "run_rollback_to_release called on a pool-backed context; \
                 this is a framework-invariant bug",
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── IsolationLevel — SQL keyword + composition tests ─────────────────
    //
    // Pins the SQL grammar at the framework boundary: a regression in
    // `as_sql_keyword` would emit malformed SQL ("BEGIN ISOLATION
    // LEVEL Serializable" is rejected — Postgres needs the SQL
    // standard's two-word forms). These are no-DB tests so they run
    // in the unit-test pass without requiring a live Postgres.

    #[test]
    fn isolation_level_keywords_match_postgres_grammar() {
        assert_eq!(
            IsolationLevel::ReadCommitted.as_sql_keyword(),
            "READ COMMITTED"
        );
        assert_eq!(
            IsolationLevel::RepeatableRead.as_sql_keyword(),
            "REPEATABLE READ"
        );
        assert_eq!(
            IsolationLevel::Serializable.as_sql_keyword(),
            "SERIALIZABLE"
        );
    }

    #[test]
    fn isolation_level_display_matches_keyword() {
        assert_eq!(IsolationLevel::ReadCommitted.to_string(), "READ COMMITTED");
        assert_eq!(
            IsolationLevel::RepeatableRead.to_string(),
            "REPEATABLE READ"
        );
        assert_eq!(IsolationLevel::Serializable.to_string(), "SERIALIZABLE");
    }

    #[test]
    fn begin_with_isolation_sql_composes_with_keyword() {
        assert_eq!(
            begin_with_isolation_sql(IsolationLevel::ReadCommitted),
            "BEGIN ISOLATION LEVEL READ COMMITTED",
        );
        assert_eq!(
            begin_with_isolation_sql(IsolationLevel::RepeatableRead),
            "BEGIN ISOLATION LEVEL REPEATABLE READ",
        );
        assert_eq!(
            begin_with_isolation_sql(IsolationLevel::Serializable),
            "BEGIN ISOLATION LEVEL SERIALIZABLE",
        );
    }

    // ── ConstraintMode — SQL keyword tests ──────────────────────────────

    #[test]
    fn constraint_mode_keywords_match_postgres_grammar() {
        assert_eq!(ConstraintMode::Deferred.as_sql_keyword(), "DEFERRED");
        assert_eq!(ConstraintMode::Immediate.as_sql_keyword(), "IMMEDIATE");
    }

    // ── DeferScope — equality + variant pin tests ───────────────────────

    #[test]
    fn defer_scope_all_is_value_equal() {
        // `DeferScope` is `Copy + Eq`. Two `All` values must compare
        // equal regardless of how the call site constructed them.
        assert_eq!(DeferScope::All, DeferScope::All);
    }

    #[test]
    fn defer_scope_named_compares_by_slice_contents() {
        // Equality on `Named` uses pointer + length equality of the
        // slice — two slices with identical contents but distinct
        // static lifetimes would be equal; the compiler dedupes
        // identical literal slices.
        static A: &[&str] = &["posts_author_id_fkey"];
        static B: &[&str] = &["posts_author_id_fkey"];
        assert_eq!(DeferScope::Named(A), DeferScope::Named(B));
    }
}
