use super::DjogiContext;

/// Guard for a pool-backed pinned migration context.
///
/// Owns a transaction-backed `DjogiContext` that was created by checking out
/// one connection from the parent's pool. On drop, if [`mark_clean`](Self::mark_clean)
/// was not called, the underlying connection is **detached** from the pool
/// (closing the socket) to prevent session poisoning of subsequent checkouts.
/// GH #331.
pub(crate) struct OwnedPinnedCtx {
    ctx: DjogiContext,
    clean: bool,
}

impl OwnedPinnedCtx {
    /// Create a new owned pinned context from a transaction-backed `DjogiContext`.
    pub(crate) fn new(ctx: DjogiContext) -> Self {
        Self { ctx, clean: false }
    }

    /// Mark this session as clean (advisory lock released).
    ///
    /// When called before drop, the underlying connection is returned to the
    /// pool normally instead of being detached. This is the happy path for
    /// migrations that complete successfully and release their locks.
    #[allow(dead_code)] // Used by migration runner after GH #331 integration
    pub(crate) fn mark_clean(&mut self) {
        self.clean = true;
    }
}

impl Drop for OwnedPinnedCtx {
    fn drop(&mut self) {
        if !self.clean {
            // Detach the connection to prevent session poisoning (stale advisory
            // locks, open transactions, etc.) from leaking back into the pool.
            self.ctx.detach_transaction_connection_mut();
        } else {
            // Clean exit: DjogiContext::drop returns the connection to the
            // pool via deadpool's normal recycling path.
        }
    }
}

impl std::ops::Deref for OwnedPinnedCtx {
    type Target = DjogiContext;

    fn deref(&self) -> &DjogiContext {
        &self.ctx
    }
}

impl std::ops::DerefMut for OwnedPinnedCtx {
    fn deref_mut(&mut self) -> &mut DjogiContext {
        &mut self.ctx
    }
}

/// Return type of [`DjogiContext::pin_for_migration`](super::DjogiContext::pin_for_migration).
///
/// Derefs to `&mut DjogiContext` so callers can pass it to any
/// function that accepts `&mut PinnedCtx<'_>`. When the migration
/// entry point drops the `PinnedCtx`, the `Owned` variant's
/// [`OwnedPinnedCtx`] guard handles detachment semantics: if
/// [`mark_clean`](OwnedPinnedCtx::mark_clean) was not called, the
/// underlying connection is detached from the pool to prevent session
/// poisoning (GH #331).
///
/// # Structural invariant
///
/// The enum variants are `pub(crate)` (Rust does not support independent
/// variant visibility — they inherit the enum's visibility). Construction
/// is restricted to this module tree by convention and code review. Only
/// [`DjogiContext::pin_for_migration`](super::DjogiContext::pin_for_migration)
/// constructs these variants (GH #331 Finality F-331-1).
#[allow(clippy::large_enum_variant)]
#[allow(dead_code)] // GH #331: unused until advisory-lock integration lands
pub(crate) enum PinnedCtx<'a> {
    /// Pool-backed context: a fresh connection was checked out and
    /// wrapped in a new `DjogiContext`. Dropping this variant runs the
    /// [`OwnedPinnedCtx`] guard — detaching if dirty, returning if clean.
    Owned(OwnedPinnedCtx),
    /// Already connection-backed: borrows the caller's context.
    Borrowed(&'a mut DjogiContext),
}

impl<'a> std::ops::Deref for PinnedCtx<'a> {
    type Target = DjogiContext;

    fn deref(&self) -> &DjogiContext {
        match self {
            PinnedCtx::Owned(guard) => guard,
            PinnedCtx::Borrowed(ctx) => ctx,
        }
    }
}

impl<'a> std::ops::DerefMut for PinnedCtx<'a> {
    fn deref_mut(&mut self) -> &mut DjogiContext {
        match self {
            PinnedCtx::Owned(guard) => guard,
            PinnedCtx::Borrowed(ctx) => ctx,
        }
    }
}

impl<'a> PinnedCtx<'a> {
    /// Mark this pinned context as clean for pool return.
    ///
    /// Only effective on `Owned` variant — marks the internal guard so that
    /// `Drop` returns the connection to the pool instead of detaching it.
    /// No-op on `Borrowed` (the borrowed context's lifecycle is managed by
    /// its parent scope).
    pub(crate) fn mark_clean(&mut self) {
        if let PinnedCtx::Owned(guard) = self {
            guard.mark_clean();
        }
        // Borrowed: no-op — the caller's context manages its own lifecycle
    }
}
#[cfg(test)]
mod tests {
    use crate::pg::pool::DjogiPool;

    /// When a pool-backed `PinnedCtx::Owned` is dropped without calling
    /// `mark_clean()`, the underlying connection must be **detached from
    /// the pool** (closing the socket) rather than returned to it.
    ///
    /// Without this guarantee, async cancellation that drops a pinned
    /// context after acquiring an advisory lock returns the poisoned
    /// connection to the pool, where a subsequent checkout inherits the
    /// stale lock and gains false mutual exclusion. GH #331.
    ///
    /// **Verification via backend PID comparison.** `pg_try_advisory_lock($k)`
    /// returns true when the SAME session already holds that lock (idempotent
    /// no-op), so a "lock succeeds" probe cannot distinguish a recycled
    /// poisoned connection from a genuinely free one. Comparing the backend
    /// PID of the pinned connection against the PID of the next pool checkout
    /// is the only reliable way to prove detachment: different PIDs mean the
    /// old physical connection was closed, not recycled.
    ///
    /// **Expected result BEFORE fix:** FAIL (assertion fails because both
    /// checkouts return the SAME PID — the connection WAS recycled).
    /// **Expected result AFTER fix:** PASS (different PIDs prove detachment).
    #[tokio::test]
    #[ignore] // Requires live Postgres via DATABASE_URL
    async fn cancellation_drop_detaches_connection_from_pool() {
        // Skip gracefully when no database is configured.
        let database_url = std::env::var("DATABASE_URL").unwrap_or_default();
        if database_url.is_empty() {
            tracing::info!(
                "cancellation_drop_detaches_connection_from_pool: \
                 DATABASE_URL not set, skipping"
            );
            return;
        }

        // Build a pool with max_size(2) so connections are recycled under
        // normal drop (not leaked to new physical connections).
        let pool = DjogiPool::builder(&database_url)
            .max_size(2)
            .build()
            .await
            .expect("pool build should succeed");

        let mut ctx = super::DjogiContext::from_pool(pool.clone());

        // 4. Pin the context — checks out conn1 from the pool.
        let mut pinned = ctx
            .pin_for_migration()
            .await
            .expect("pin_for_migration should checkout a connection");

        // 5. Record the backend PID of the pinned connection.
        let old_pid_row = pinned
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .expect("pg_backend_pid query should succeed");
        let old_pid: i32 = old_pid_row
            .try_get(0)
            .expect("pg_backend_pid column should be decodable as i32");

        // 6. Acquire a session-level advisory lock to prove the connection
        //    is in a non-default state. The key is a fixed constant unique
        //    to this test (will not collide with runner-derived keys).
        let _lock_row = pinned
            .query_one(
                "SELECT pg_try_advisory_lock($1)",
                &[&(998877665544332211i64)],
            )
            .await
            .expect("advisory lock acquisition should succeed");

        // 7. Drop the pinned context WITHOUT mark_clean().
        //    This simulates async cancellation dropping the future mid-flight.
        drop(pinned);

        // Small yield to allow the connection to be returned/detached and
        // the pool state to stabilize before the next checkout.
        tokio::task::yield_now().await;

        // 8. Get a NEW connection from the same pool via direct checkout.
        let mut new_conn = pool
            .get()
            .await
            .expect("pool get should succeed after pinned drop");

        // 9. Record the backend PID of the fresh connection.
        let new_pid_row = new_conn
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .expect("pg_backend_pid query on fresh connection should succeed");
        let new_pid: i32 = new_pid_row
            .try_get(0)
            .expect("pg_backend_pid column should be decodable as i32");

        // 10. Assert different PIDs — proves the old connection was detached
        //     (closed), not recycled back into the pool with stale state.
        assert_ne!(
            old_pid, new_pid,
            "BUG (GH #331): dropping PinnedCtx::Owned without mark_clean() \
             returned the connection to the pool instead of detaching it. \
             Old PID {old_pid} == New PID {new_pid} means the same physical \
             connection was recycled with the advisory lock still held, \
             poisoning subsequent checkouts."
        );
    }
}
