//! `atomic(...)` — the canonical transaction scope + retry helper.
//!
//! # `atomic()` at a glance
//!
//! `atomic(&pool, |ctx| async move { ... })` is the outermost entry
//! point: it calls `pool.begin()`, wraps the transaction in a fresh
//! [`DjogiContext`](crate::DjogiContext), runs the closure, commits on
//! `Ok`, rolls back on `Err`, and drains the on-commit callback queue
//! after a successful commit. Nested calls —
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
//! `Transaction::rollback` or `ROLLBACK TO SAVEPOINT`) **before** the
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

use crate::DjogiError;
use crate::context::{ContextInner, DjogiContext};
use crate::error::is_lock_error;
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
///
/// Mirrors the pattern `sqlx::Connection::transaction` uses — see the
/// sqlx source for the same shape.
pub type AtomicFuture<'a, R> = Pin<Box<dyn Future<Output = Result<R, DjogiError>> + Send + 'a>>;

// ---------------------------------------------------------------------------
// Sealed trait — `&PgPool` and `&mut DjogiContext` are the only scopes.
// ---------------------------------------------------------------------------

mod sealed {
    pub trait Sealed {}
    impl Sealed for &sqlx::PgPool {}
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
    ///
    /// `#[doc(hidden)]` on the method surface: callers go through
    /// [`atomic()`].
    ///
    /// The returned future is not `+ Send` bounded on the trait — the
    /// impls are concrete `async fn`s and infer `Send`-ness from the
    /// closure the caller supplies. Bounding the trait return would
    /// force the caller's `Send`-non-`Send` properties through a
    /// contract boundary; leaving it open lets both flavours compose.
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

impl IntoAtomicScope for &sqlx::PgPool {
    async fn run_atomic<F, R>(self, closure: F) -> Result<R, DjogiError>
    where
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send,
    {
        let tx = self.begin().await.map_err(DjogiError::from)?;
        let mut ctx = DjogiContext::from_transaction(tx);

        // Poll the closure through `catch_unwind` so a panic turns into
        // a caught payload. See the module-level panic-semantics docs.
        let result = AssertUnwindSafe(closure(&mut ctx)).catch_unwind().await;

        match result {
            Ok(Ok(value)) => {
                // Closure succeeded — commit and drain on-commit queue.
                // Commit consumes the context and drains panic-safely
                // (see `DjogiContext::commit`).
                ctx.commit().await?;
                Ok(value)
            }
            Ok(Err(err)) => {
                // Closure returned Err — roll the transaction back and
                // surface the original error. The rollback itself may
                // fail (dropped connection, etc.); in that case the
                // original error is what the user cares about, so we
                // log the rollback failure and return the closure err.
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
            return Err(DjogiError::Sqlx(sqlx::Error::Configuration(
                "atomic(&mut ctx, ...) requires a transaction-backed context; \
                 wrap the outermost call in atomic(&pool, ...)"
                    .into(),
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
            ContextInner::Transaction(tx) => sqlx::query(&sp_sql).execute(&mut **tx).await,
            // Unreachable because of the guard above, but keep the
            // exhaustive match so a future `ContextInner` variant
            // surfaces at compile time.
            ContextInner::Pool(_) => unreachable!("guard above rules this out"),
        };
        if let Err(e) = push_result {
            self.decrement_savepoint_depth();
            return Err(DjogiError::from(e));
        }

        // Nested path shares the parent context directly — inner
        // writes land on the same transaction, inner on_commit
        // callbacks land on the parent's queue. To honour the
        // "discard on rollback" half of the promote-on-Ok semantics
        // we snapshot the parent's callback-queue length before
        // entering the closure and truncate back to it on the Err /
        // panic branches.
        let callbacks_before = self.on_commit_queue_len();

        let inner_result = AssertUnwindSafe(closure(self)).catch_unwind().await;

        match inner_result {
            Ok(Ok(value)) => {
                // Success — RELEASE SAVEPOINT. Inner callbacks stay on
                // the parent queue (promoted).
                let release_sql = format!("RELEASE SAVEPOINT {savepoint_name}");
                let release_res = match self.inner_mut() {
                    ContextInner::Transaction(tx) => {
                        sqlx::query(&release_sql).execute(&mut **tx).await
                    }
                    ContextInner::Pool(_) => unreachable!(),
                };
                self.decrement_savepoint_depth();
                release_res.map_err(DjogiError::from)?;
                Ok(value)
            }
            Ok(Err(err)) => {
                // Closure returned Err — ROLLBACK TO SAVEPOINT then
                // RELEASE. Discard inner callbacks by truncating the
                // parent queue back to its pre-closure length.
                self.truncate_on_commit_queue(callbacks_before);
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
                // pool impl, scoped to the savepoint.
                self.truncate_on_commit_queue(callbacks_before);
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
/// Classifies errors via [`crate::error::is_lock_error`] — SQLSTATEs
/// `40001`, `40P01`, `55P03` are considered retryable. Every other
/// error (constraint violations, not-found, etc.) surfaces on the
/// first call. Pure retry with no backoff in Phase 4 Task 1; Task 7
/// will add exponential / jittered backoff together with the full
/// `DjogiError::LockConflict` variant.
///
/// The closure is invoked with `&mut DjogiContext` so it can be used
/// either inside an existing `atomic()` scope (where `ctx` already
/// carries a transaction) or with a pool-backed context. On a
/// transaction-backed context the retry re-runs the closure against
/// the SAME transaction — useful for idempotent logical retries
/// inside a single transaction; on a pool-backed context each retry
/// still shares the context. Callers that need a fresh transaction
/// per attempt should compose `retry_on_conflict` with `atomic()`
/// at the call site.
///
/// # Panics
///
/// A closure panic propagates on the current attempt; panics are not
/// classified as retryable.
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
                let retryable = matches!(&e, DjogiError::Sqlx(sqlx_err) if is_lock_error(sqlx_err));
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
//
// Declared here rather than in `context.rs` because they exist solely
// to support the nested `atomic()` dispatch; keeping them co-located
// with the only caller makes the coupling obvious.
// ---------------------------------------------------------------------------

impl DjogiContext {
    /// Current length of the on-commit callback queue. Used by the
    /// nested `atomic()` impl to snapshot the queue before entering
    /// the closure so inner-registered callbacks can be dropped on
    /// rollback.
    pub(crate) fn on_commit_queue_len(&self) -> usize {
        self.on_commit_len()
    }

    /// Truncate the on-commit callback queue back to `new_len`. Used
    /// by the nested `atomic()` impl on the rollback / panic branches
    /// to discard callbacks registered inside the inner scope.
    pub(crate) fn truncate_on_commit_queue(&mut self, new_len: usize) {
        self.on_commit_truncate(new_len);
    }

    /// Issue `ROLLBACK TO SAVEPOINT` followed by `RELEASE SAVEPOINT`.
    /// Returns `Some(err)` on the first failure, `None` if both
    /// succeeded. Used by the nested impl on the Err / panic branches.
    ///
    /// The RELEASE is still issued after a successful ROLLBACK so the
    /// savepoint name is reusable; a failed ROLLBACK short-circuits
    /// since a subsequent RELEASE against a non-existent savepoint
    /// would return its own error and obscure the original.
    async fn run_rollback_to_release(
        &mut self,
        rb_sql: &str,
        rel_sql: &str,
    ) -> Option<sqlx::Error> {
        match self.inner_mut() {
            ContextInner::Transaction(tx) => {
                if let Err(e) = sqlx::query(rb_sql).execute(&mut **tx).await {
                    return Some(e);
                }
                if let Err(e) = sqlx::query(rel_sql).execute(&mut **tx).await {
                    return Some(e);
                }
                None
            }
            ContextInner::Pool(_) => Some(sqlx::Error::Configuration(
                "run_rollback_to_release called on a pool-backed context; \
                 this is a framework-invariant bug"
                    .into(),
            )),
        }
    }
}
