//! Row-level locking for SELECT queries — `FOR UPDATE` and its
//! NOWAIT / SKIP LOCKED variants.
//!
//! Phase 4 Task 7 adds lock-mode state to [`QuerySet`](crate::query::QuerySet)
//! and emits the matching SQL tail during [`crate::query::sql::build_select`].
//! The three non-default variants map onto Postgres' row-locking clauses
//! in the read path:
//!
//! | Variant               | Emits                     | Behaviour on contention              |
//! |-----------------------|---------------------------|--------------------------------------|
//! | `None` (default)      | (no tail)                 | ordinary SELECT, no lock             |
//! | `ForUpdate`           | `FOR UPDATE`              | blocks until the lock is released    |
//! | `ForUpdateNowait`     | `FOR UPDATE NOWAIT`       | errors immediately with SQLSTATE 55P03 |
//! | `ForUpdateSkipLocked` | `FOR UPDATE SKIP LOCKED`  | silently skips rows locked elsewhere |
//!
//! # Pool-backed `FOR UPDATE` is a footgun — surface it loudly
//!
//! A `FOR UPDATE` lock is held until the end of the enclosing
//! transaction. A pool-backed [`DjogiContext`](crate::DjogiContext)
//! auto-commits each statement, so a `SELECT ... FOR UPDATE` on a
//! pool-backed context acquires the lock, then releases it instantly
//! when the implicit transaction closes — **no protection whatsoever**
//! against a concurrent writer between `fetch_*` and `save`.
//!
//! Terminal methods that execute the SELECT do NOT reject pool-backed
//! contexts when a non-`None` lock is set — that would be a runtime
//! error for a correctness question the caller can answer at the call
//! site. Instead, the rustdoc on every lock builder calls the
//! constraint out explicitly, and the integration suite pins the
//! `FOR UPDATE NOWAIT` semantic to an `atomic()` scope so the default
//! code path is correct. Callers who lock outside of `atomic()` are
//! expected to know what they are doing.

/// Row-level lock mode accumulated on a [`QuerySet`](crate::query::QuerySet).
///
/// Crate-private: users configure the lock via the typed builder
/// methods (`select_for_update` / `nowait` / `skip_locked`) which gate
/// the variant transitions. Keeping the enum itself `pub(crate)`
/// prevents downstream code from constructing illegal combinations
/// (e.g. `ForUpdateNowait` without first calling `select_for_update`).
///
/// `Default` is `None` so a fresh `QuerySet<T>` carries no lock tail
/// — the same behaviour shipped before Task 7 landed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum LockMode {
    /// No row-level lock. Ordinary `SELECT` with no trailing clause.
    #[default]
    None,
    /// `FOR UPDATE` — acquire an exclusive row lock for the duration
    /// of the enclosing transaction. Blocks until the lock is
    /// released if another session already holds it.
    ForUpdate,
    /// `FOR UPDATE NOWAIT` — acquire the lock if available, else
    /// return immediately with Postgres SQLSTATE `55P03`
    /// (`lock_not_available`), which
    /// [`DjogiError::LockConflict`](crate::DjogiError::LockConflict)
    /// classifies.
    ForUpdateNowait,
    /// `FOR UPDATE SKIP LOCKED` — silently skip rows locked by
    /// another session, returning only unlocked rows. The typical
    /// shape for work-queue consumers.
    ForUpdateSkipLocked,
}

impl LockMode {
    /// Append the row-lock tail to the query builder, or no-op for
    /// [`LockMode::None`].
    ///
    /// Crate-private because only [`crate::query::sql::build_select`]
    /// and the joined-select variant emit row locks — terminals
    /// (`count`, `exists`, aggregates) never carry a lock because they
    /// don't return rows the caller can hold open against.
    pub(crate) fn push_tail(self, qb: &mut sqlx::QueryBuilder<'_, sqlx::Postgres>) {
        match self {
            LockMode::None => {}
            LockMode::ForUpdate => {
                qb.push(" FOR UPDATE");
            }
            LockMode::ForUpdateNowait => {
                qb.push(" FOR UPDATE NOWAIT");
            }
            LockMode::ForUpdateSkipLocked => {
                qb.push(" FOR UPDATE SKIP LOCKED");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{Postgres, QueryBuilder};

    #[test]
    fn none_emits_no_tail() {
        let mut qb: QueryBuilder<'_, Postgres> = QueryBuilder::new("");
        LockMode::None.push_tail(&mut qb);
        assert_eq!(qb.sql(), "");
    }

    #[test]
    fn for_update_emits_bare_clause() {
        let mut qb: QueryBuilder<'_, Postgres> = QueryBuilder::new("");
        LockMode::ForUpdate.push_tail(&mut qb);
        assert_eq!(qb.sql().trim(), "FOR UPDATE");
    }

    #[test]
    fn for_update_nowait_emits_nowait() {
        let mut qb: QueryBuilder<'_, Postgres> = QueryBuilder::new("");
        LockMode::ForUpdateNowait.push_tail(&mut qb);
        assert_eq!(qb.sql().trim(), "FOR UPDATE NOWAIT");
    }

    #[test]
    fn for_update_skip_locked_emits_skip_locked() {
        let mut qb: QueryBuilder<'_, Postgres> = QueryBuilder::new("");
        LockMode::ForUpdateSkipLocked.push_tail(&mut qb);
        assert_eq!(qb.sql().trim(), "FOR UPDATE SKIP LOCKED");
    }

    #[test]
    fn default_is_none() {
        assert_eq!(LockMode::default(), LockMode::None);
    }
}
