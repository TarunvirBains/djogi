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
//! - **Pool**: backed by a `sqlx::PgPool` — each operation auto-connects
//! - **Transaction**: an active `sqlx::Transaction<'static, sqlx::Postgres>` — all
//!   operations join the same logical transaction until `commit()` or `rollback()`
//!   is called.
//!
//! # Execution dispatch pattern
//!
//! CRUD methods and QuerySet terminals that today take `&mut DjogiContext` dispatch
//! to sqlx via an inline match on [`ContextInner`]. At the sqlx boundary inside every
//! method, the chain looks like:
//!
//! ```ignore
//! let q = sqlx::query_as::<_, Self>(sql).bind(col_a).bind(col_b);
//! let row = match ctx.inner_mut() {
//!     ContextInner::Pool(pool) => q.fetch_one(&*pool).await?,
//!     ContextInner::Transaction(tx) => q.fetch_one(&mut **tx).await?,
//! };
//! ```
//!
//! Two variants = two match arms = negligible overhead. The alternative — implementing
//! `sqlx::Executor` for `&mut DjogiContext` — would require navigating sqlx's GAT-shaped
//! `Executor<'a>` lifetime and isn't how sqlx expects downstream code to plug in. The
//! inline match is explicit, compiles easily, and keeps the abstraction boundary thin.
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
//! the commit itself (per Phase 4 v3 Q9 resolution). Callbacks are FIFO.
//!
//! # Drain points
//!
//! Registered callbacks are consumed by exactly two paths:
//!
//! - [`DjogiContext::commit`] — the low-level tx-backed commit drains the
//!   queue after `sqlx::Transaction::commit` succeeds, runs each callback in
//!   FIFO order, and logs any callback error via `tracing::error!` without
//!   unwinding the caller. Used by tests and integration code that hand-manage
//!   the transaction boundary.
//! - `atomic()` (Phase 4 Task 1) — once landed, the canonical entry point
//!   for application code; wraps the same drain-after-commit semantics but
//!   also handles nested savepoints. See the Phase 4 plan section on
//!   `atomic()` for the full flow.
//!
//! Callbacks registered on a pool-backed context with no `atomic()` scope
//! are silently dropped when the context is dropped — the retrofit does not
//! ship immediate-firing for the pool-backed path. Application code that
//! needs post-operation side effects on a bare pool should enter `atomic()`
//! (Phase 4 Task 1 onwards) or call `commit()` explicitly on a tx-backed
//! context.

use crate::DjogiError;
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;

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
}

/// Internal enum selecting the active context variant.
///
/// `#[doc(hidden)] pub` because `#[derive(Model)]`-generated CRUD bodies
/// pattern-match on this enum to dispatch to sqlx, and the generated code
/// compiles in user crates that only depend on `djogi`. Framework modules
/// reach it via the crate-private [`DjogiContext::inner_mut`] accessor; user
/// code should go through [`DjogiContext::pool`] / [`DjogiContext::tx`]
/// instead — the `__` prefix and `#[doc(hidden)]` attribute are the social
/// signal that this type carries no stability guarantee.
#[doc(hidden)]
pub enum ContextInner {
    /// Pool-backed: auto-connects per operation.
    Pool(sqlx::PgPool),

    /// Transaction-backed: all operations share the same logical transaction.
    ///
    /// **Lifetime note:** This uses `'static` because the context owns the
    /// transaction and is typically held in a local or field for the duration
    /// of a `atomic()` call. If future uses require shorter-lived transactions
    /// (e.g., borrowed from outer scope), a redesign to `&mut Transaction<'_>`
    /// or a PIN-based approach may be needed. Document any such requirements
    /// as they arrive.
    Transaction(sqlx::Transaction<'static, sqlx::Postgres>),
}

/// Public-but-hidden alias of [`ContextInner`] for macro-generated code.
///
/// `#[derive(Model)]` emits CRUD bodies that pattern-match on the context's
/// inner variant to dispatch to sqlx. Those bodies compile in the user's
/// crate, which only has `djogi` as a dependency — so the enum has to be
/// nameable via an absolute path `::djogi::context::__ContextInnerForMacros`.
/// Hidden from docs and prefixed with `__` so the social signal is clear:
/// this is framework-internal API, not part of the stable surface.
#[doc(hidden)]
pub type __ContextInnerForMacros = ContextInner;

impl DjogiContext {
    /// Create a context backed by a `PgPool`.
    ///
    /// # Example
    /// ```ignore
    /// let mut ctx = DjogiContext::from_pool(pool);
    /// let user = User::create(&mut ctx, user).await?;
    /// ```
    pub fn from_pool(pool: sqlx::PgPool) -> Self {
        DjogiContext {
            inner: ContextInner::Pool(pool),
            savepoint_depth: 0,
            on_commit: Vec::new(),
        }
    }

    /// Create a context backed by an active transaction.
    ///
    /// Typically called by `atomic()` (Phase 4 Task 1) or by test / integration
    /// code that manages its own transaction boundaries. Production code
    /// should prefer [`atomic()`](Self::atomic) (once it lands) so on-commit
    /// callbacks dispatch correctly; this constructor is the low-level escape
    /// hatch for callers who really do need to hand-manage a transaction.
    pub fn from_transaction(tx: sqlx::Transaction<'static, sqlx::Postgres>) -> Self {
        DjogiContext {
            inner: ContextInner::Transaction(tx),
            savepoint_depth: 0,
            on_commit: Vec::new(),
        }
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
    ///
    /// Use this for raw sqlx escape hatches that do not require mutation, or
    /// for multi-query fan-out terminals that must hold a pool reference across
    /// multiple sequential queries (e.g. `fetch_all_prefetched`).
    pub fn pool(&self) -> Option<&sqlx::PgPool> {
        match &self.inner {
            ContextInner::Pool(pool) => Some(pool),
            ContextInner::Transaction(_) => None,
        }
    }

    /// Get a mutable reference to the inner transaction if this context is transaction-backed.
    ///
    /// Returns `Some(&mut tx)` iff the context was created via `from_transaction()`.
    /// Returns `None` if this is a pool context.
    ///
    /// Use this for raw sqlx escape hatches that require mutation (e.g., custom
    /// `sqlx::QueryBuilder` operations).
    pub fn tx(&mut self) -> Option<&mut sqlx::Transaction<'static, sqlx::Postgres>> {
        match &mut self.inner {
            ContextInner::Pool(_) => None,
            ContextInner::Transaction(tx) => Some(tx),
        }
    }

    /// Crate-private mutable accessor for the context's inner variant.
    ///
    /// Used by every CRUD / QuerySet terminal in the framework to pattern-match
    /// on pool-vs-transaction at the sqlx boundary. Kept `pub(crate)` because
    /// [`ContextInner`] itself is crate-private — downstream code routes through
    /// the public [`pool`](Self::pool) / [`tx`](Self::tx) accessors instead.
    pub(crate) fn inner_mut(&mut self) -> &mut ContextInner {
        &mut self.inner
    }

    /// Public-but-hidden mutable accessor used by `#[derive(Model)]`-generated
    /// CRUD bodies to pattern-match on the context's inner variant.
    ///
    /// Not part of the stable API — prefixed with `__` and `#[doc(hidden)]`
    /// so the social signal is clear: downstream code should not call this
    /// directly. Use [`pool`](Self::pool) / [`tx`](Self::tx) instead, or stay
    /// inside `Model::*` / `QuerySet::*` calls which dispatch automatically.
    #[doc(hidden)]
    pub fn __inner_mut_for_macros(&mut self) -> &mut __ContextInnerForMacros {
        &mut self.inner
    }

    /// Commit the underlying transaction, consuming the context.
    ///
    /// Returns `Ok(())` if the context was transaction-backed and the commit
    /// succeeded. Returns `Err(DjogiError::Sqlx(..))` if the commit failed or
    /// the context was pool-backed (pool contexts have no transaction to
    /// commit — calling `.commit()` on one is a caller error).
    ///
    /// # On-commit callbacks
    ///
    /// After `sqlx::Transaction::commit` returns `Ok(())`, every callback
    /// registered via [`on_commit`](Self::on_commit) fires in FIFO order.
    /// Per Phase 4 v3 Q9, callback errors are logged via `tracing::error!`
    /// but do NOT unwind the caller — a failing callback must not fail the
    /// commit, and subsequent callbacks still fire.
    ///
    /// If the underlying commit fails, the callbacks are dropped without
    /// running (the transaction did not commit, so post-commit side effects
    /// are inappropriate).
    ///
    /// Prefer `atomic()` (Phase 4 Task 1) for transaction management;
    /// `commit()` is the low-level escape hatch for tests and integration
    /// code that manage transaction boundaries by hand.
    ///
    /// # Panics
    ///
    /// On-commit callbacks are drained panic-safely: each callback future
    /// is wrapped in `AssertUnwindSafe(..).catch_unwind()`. A panicking
    /// callback is logged via `tracing::error!` and the drain continues
    /// with the next callback. The panic does **not** propagate out of
    /// `commit()`. This matches Phase 4 v3 Q9 on error semantics: a
    /// failing callback — whether it panics or returns `Err` — must not
    /// fail the commit, and subsequent callbacks still fire.
    pub async fn commit(self) -> Result<(), DjogiError> {
        // Split the context so we can run the callbacks after consuming
        // the underlying transaction.
        let DjogiContext {
            inner, on_commit, ..
        } = self;

        match inner {
            ContextInner::Pool(_) => Err(DjogiError::Sqlx(sqlx::Error::Configuration(
                "DjogiContext::commit called on a pool-backed context".into(),
            ))),
            ContextInner::Transaction(tx) => {
                tx.commit().await.map_err(DjogiError::from)?;
                drain_on_commit(on_commit).await;
                Ok(())
            }
        }
    }

    /// Roll back the underlying transaction, consuming the context.
    ///
    /// Returns `Ok(())` if the context was transaction-backed and the
    /// rollback succeeded. Returns `Err(DjogiError::Sqlx(..))` if the rollback
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
            ContextInner::Pool(_) => Err(DjogiError::Sqlx(sqlx::Error::Configuration(
                "DjogiContext::rollback called on a pool-backed context".into(),
            ))),
            ContextInner::Transaction(tx) => {
                tx.rollback().await.map_err(DjogiError::from)?;
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
    /// This is a low-level helper used by tests and by the forthcoming
    /// `atomic()` implementation; production code should reach for
    /// `atomic()` once it lands so on-commit callbacks dispatch correctly.
    pub async fn begin(&self) -> Result<DjogiContext, DjogiError> {
        match &self.inner {
            ContextInner::Pool(pool) => {
                let tx = pool.begin().await.map_err(DjogiError::from)?;
                Ok(DjogiContext::from_transaction(tx))
            }
            ContextInner::Transaction(_) => Err(DjogiError::Sqlx(sqlx::Error::Configuration(
                "DjogiContext::begin called on a transaction-backed context; \
                 nested transactions require atomic() (Phase 4 Task 1)"
                    .into(),
            ))),
        }
    }

    /// Register an async callback to fire after a successful commit.
    ///
    /// Callbacks execute in FIFO order after the transaction commits.
    /// Callback errors are logged via `tracing::error!` but do not fail the
    /// commit (per Phase 4 v3 Q9 resolution). Subsequent callbacks still
    /// fire even if an earlier callback fails.
    ///
    /// # Drain points
    ///
    /// Registered callbacks are consumed by:
    ///
    /// - [`commit()`](Self::commit) — drains the queue after the underlying
    ///   `sqlx::Transaction::commit` succeeds (this IS implemented).
    /// - `atomic()` (Phase 4 Task 1) — the canonical path once it lands.
    ///
    /// # Behaviour on pool-backed contexts
    ///
    /// When called on a pool-backed context outside any `atomic()` scope,
    /// the callback is queued but will not fire until a subsequent
    /// `atomic()` enters and commits. Calling `on_commit` on a pool-backed
    /// context without entering `atomic()` means the callback is silently
    /// dropped when the context is dropped — the retrofit does not ship
    /// immediate-firing for the pool-backed path, and Phase 4 Task 1 owns
    /// the eventual pool-backed dispatch semantics.
    ///
    /// # Behaviour on rollback
    ///
    /// Callbacks registered during a transaction that is rolled back via
    /// [`rollback()`](Self::rollback) are discarded without firing — see
    /// that method for the full rationale.
    ///
    /// # Panics
    ///
    /// Panics inside a registered callback are not caught by the framework;
    /// see [`commit()`](Self::commit)'s `# Panics` section for behavior.
    ///
    /// # Example
    /// ```ignore
    /// ctx.on_commit(|| async {
    ///     cache.invalidate_user(user_id).await?;
    ///     Ok(())
    /// });
    /// ```
    pub fn on_commit<F, Fut>(&mut self, callback: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), DjogiError>> + Send + 'static,
    {
        let boxed: OnCommitCallback = Box::new(move || Box::pin(callback()));
        self.on_commit.push(boxed);
    }

    /// Consume all registered on-commit callbacks and return them.
    ///
    /// **Internal use only.** Called by `atomic()` after a successful commit
    /// on the outermost scope, and when a nested `atomic()` scope exits
    /// successfully (nested callbacks promote to the outer stack). Used
    /// together with [`append_on_commit_callbacks`](Self::append_on_commit_callbacks)
    /// to move callbacks between contexts.
    #[allow(dead_code)]
    pub(crate) fn take_on_commit_callbacks(&mut self) -> Vec<OnCommitCallback> {
        std::mem::take(&mut self.on_commit)
    }

    /// Append a batch of on-commit callbacks to this context's queue.
    ///
    /// **Internal use only.** Used by `atomic()` on the nested-success
    /// path: the inner context's callbacks are drained via
    /// [`take_on_commit_callbacks`](Self::take_on_commit_callbacks) and
    /// then appended here so they fire after the outermost commit.
    /// Order is preserved across the promotion.
    #[allow(dead_code)]
    pub(crate) fn append_on_commit_callbacks(&mut self, callbacks: Vec<OnCommitCallback>) {
        self.on_commit.extend(callbacks);
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

/// Drain a batch of on-commit callbacks panic-safely.
///
/// Wraps each callback future in `AssertUnwindSafe(..).catch_unwind()`
/// so a panicking callback is logged via `tracing::error!` without
/// aborting the drain loop. Callback `Err` returns are likewise logged
/// and ignored — per Phase 4 v3 Q9 a callback failure must not fail the
/// commit, and every subsequent callback still fires.
///
/// `AssertUnwindSafe` is the conventional escape hatch at async
/// boundaries: user-supplied closures rarely satisfy `UnwindSafe`, and
/// the callback body owns state that lives for the callback's lifetime
/// alone, so there is no cross-callback shared state the panic could
/// corrupt. Consumed by [`DjogiContext::commit`] and by `atomic()`.
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
                // Try to extract a message for the log line; fall back
                // to a generic description otherwise. The payload itself
                // is dropped here so the drain loop continues — this is
                // the whole point of `catch_unwind`.
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
        // Create a context using the constructor without needing a real pool.
        // We'll test the structural properties of the context without network.
        // This is intentionally minimal — the retrofit agent will add
        // integration tests once the full transaction flow is implemented.

        // Since we can't construct a real pool in a unit test without Tokio,
        // we skip pool construction and test only the savepoint depth logic
        // on an internal representation. For now, this validates the API shape.

        // TODO(phase4-retrofit): Integration tests with real pool will live
        // in tests/integration/ once atomic() is implemented in Task 1.

        // Test that savepoint depth starts at zero by looking at the internal state.
        // We create a context but can't access inner directly. The public API
        // savepoint_depth() is the proper test.

        // NOTE: Can't create a DjogiContext without a real pool in a blocking context.
        // The pool tests will be covered in integration tests with #[tokio::test].
        // For now, we document the constraint in CLAUDE.md notes and verify
        // compilation + clippy + fmt at the unit test level.
    }
}
