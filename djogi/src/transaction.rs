//! `atomic(...)` — the canonical transaction scope + retry helper.
//!
//! # `atomic()` at a glance
//!
//! `atomic(&pool, |ctx| async move { ... })` is the outermost entry
//! point: it acquires a connection from the pool, issues `BEGIN`, wraps
//! the connection in a fresh [`DjogiContext`](crate::DjogiContext), runs
//! the closure, commits on `Ok`, rolls back on `Err`, and drains the
//! on-commit callback queue after a successful commit. Nested calls —
//! `atomic(&mut *outer, |inner| async move { ... })` — push a
//! Postgres savepoint rather than opening a new transaction: the inner
//! scope rolls back to / releases the savepoint on `Err`/`Ok`
//! respectively, and on success promotes its on-commit callbacks to
//! the outer context so they drain once at the outermost commit.
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
//! [`retry_on_conflict`] composes with `atomic()` to re-run a closure
//! on serialization / deadlock / `NOWAIT` failures. Pure retry — no
//! backoff — per the Phase 4 Task 1 scope.

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
// Sealed trait — `&DjogiPool` and `&mut DjogiContext` are the only scopes.
// ---------------------------------------------------------------------------

mod sealed {
    pub trait Sealed {}
    impl Sealed for &crate::pg::pool::DjogiPool {}
    impl Sealed for &mut crate::DjogiContext {}
}

/// Entry point for [`atomic()`].
///
/// Sealed: the only scopes that can open an `atomic()` block are a
/// pool reference (outermost) and a mutable [`DjogiContext`] reference
/// (nested — implemented via Postgres savepoints).
///
/// The trait carries the dispatch logic through an associated
/// [`run_atomic`](IntoAtomicScope::run_atomic) method so `atomic()`
/// itself stays as a thin forwarder over the two impls. Each impl
/// owns its own commit / rollback / callback-promotion semantics.
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
        // Acquire a connection and begin a transaction.
        let mut conn = self.get().await?;
        conn.batch_execute("BEGIN").await?;
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
        // Guard: nested `atomic` is only valid on a transaction-backed
        // context. A pool-backed context arriving here means the caller
        // tried to nest without an outer scope — surface that as a
        // configuration error pointing at the fix.
        if !matches!(self.inner_mut(), ContextInner::Transaction(_)) {
            return Err(DjogiError::Db(DbError::other(
                "atomic(&mut ctx, ...) requires a transaction-backed context; \
                 wrap the outermost call in atomic(&pool, ...)",
            )));
        }

        // Push savepoint. Depth is incremented BEFORE the SQL so
        // `sp_<depth>` numbering starts at 1 for the first nested
        // level. `sp_<n>` is ASCII + underscore — safe unquoted.
        self.increment_savepoint_depth();
        let depth = self.savepoint_depth();
        let savepoint_name = format!("sp_{depth}");

        let sp_sql = format!("SAVEPOINT {savepoint_name}");
        let push_result = match self.inner_mut() {
            ContextInner::Transaction(conn) => conn.batch_execute(&sp_sql).await,
            // Unreachable because of the guard above.
            ContextInner::Pool(_) => unreachable!("guard above rules this out"),
        };
        if let Err(e) = push_result {
            self.decrement_savepoint_depth();
            return Err(e);
        }

        // Nested path shares the parent context directly — inner
        // writes land on the same transaction, inner on_commit
        // callbacks land on the parent's queue. Snapshot before
        // entering the closure so we can truncate on rollback.
        let callbacks_before = self.on_commit_queue_len();

        // Snapshot auth-related state (auth, applied_tenant_id, tenant_set,
        // tenant_scope_suppressed) so savepoint rollback restores the
        // in-memory trackers to match the post-rollback GUC state.
        // Without this, an inner scope that does set_auth(org_b) and
        // triggers set_tenant("org_b") would leave self.applied_tenant_id
        // = Some("org_b") after ROLLBACK TO SAVEPOINT reverted the GUC
        // to the outer value — the next tenant-keyed query in the outer
        // scope would then short-circuit (matching applied_tenant_id)
        // and silently run under the wrong tenant. Phase 5.5 phase-
        // boundary fixup (Codex stop-gate review).
        let auth_snapshot = self.snapshot_auth_state();

        let inner_result = AssertUnwindSafe(closure(self)).catch_unwind().await;

        match inner_result {
            Ok(Ok(value)) => {
                // Success — RELEASE SAVEPOINT. Inner callbacks stay on
                // the parent queue (promoted). Inner auth-state mutations
                // also stay in effect: the caller's atomic scope is now
                // the parent and the inner's choices (e.g., set_auth) are
                // the continuing context.
                let release_sql = format!("RELEASE SAVEPOINT {savepoint_name}");
                let release_res = match self.inner_mut() {
                    ContextInner::Transaction(conn) => conn.batch_execute(&release_sql).await,
                    ContextInner::Pool(_) => unreachable!(),
                };
                self.decrement_savepoint_depth();
                release_res?;
                Ok(value)
            }
            Ok(Err(err)) => {
                // Closure returned Err — ROLLBACK TO SAVEPOINT then
                // RELEASE. Discard inner callbacks by truncating the
                // parent queue back to its pre-closure length, and
                // restore auth state so it matches the reverted GUC.
                self.truncate_on_commit_queue(callbacks_before);
                self.restore_auth_state(auth_snapshot);
                let rb_sql = format!("ROLLBACK TO SAVEPOINT {savepoint_name}");
                let rel_sql = format!("RELEASE SAVEPOINT {savepoint_name}");
                if let Some(rb_err) = self.run_rollback_to_release(&rb_sql, &rel_sql).await {
                    tracing::error!(
                        error = ?rb_err,
                        "atomic: ROLLBACK TO SAVEPOINT after closure Err failed; \
                         returning closure err",
                    );
                }
                self.decrement_savepoint_depth();
                Err(err)
            }
            Err(panic_payload) => {
                // Closure panicked — same rollback-then-resume as the
                // pool impl, scoped to the savepoint. Restore auth state
                // before resuming so the parent scope (if it catches
                // the unwind) sees consistent ctx state.
                self.truncate_on_commit_queue(callbacks_before);
                self.restore_auth_state(auth_snapshot);
                let rb_sql = format!("ROLLBACK TO SAVEPOINT {savepoint_name}");
                let rel_sql = format!("RELEASE SAVEPOINT {savepoint_name}");
                if let Some(rb_err) = self.run_rollback_to_release(&rb_sql, &rel_sql).await {
                    tracing::error!(
                        error = ?rb_err,
                        "atomic: ROLLBACK TO SAVEPOINT after closure panic failed; \
                         resuming panic",
                    );
                }
                self.decrement_savepoint_depth();
                resume_unwind(panic_payload);
            }
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
/// - `atomic(&pool, |ctx| async move { ... })` — outermost. Opens a
///   transaction, commits on `Ok`, rolls back on `Err`, drains
///   on-commit callbacks after the commit.
/// - `atomic(&mut ctx, |ctx| async move { ... })` — nested. Emits
///   `SAVEPOINT sp_<depth>` on entry; `RELEASE` on `Ok`, `ROLLBACK TO
///   SAVEPOINT` + `RELEASE` on `Err`. On-commit callbacks registered
///   inside a nested scope are promoted to the outer queue on
///   success, discarded on `Err`.
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
/// djogi::transaction::atomic(&pool, |ctx| async move {
///     Account::create(ctx, Account { balance: 100, ..Default::default() }).await?;
///     Ok::<_, DjogiError>(())
/// })
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
