//! Row-level locking for SELECT queries — `FOR UPDATE` / `FOR SHARE`
//! and their NOWAIT / SKIP LOCKED variants.
//! Added lock-mode state to [`QuerySet`](crate::query::QuerySet)
//! for the FOR UPDATE family; added the FOR SHARE
//! family on the same enum so the SQL emitter has a single tail
//! emitter for every row-lock shape. The matching tail is appended
//! during [`crate::query::sql::build_select`]. The six non-default
//! variants map onto Postgres' row-locking clauses in the read path:
//! | Variant | Emits | Behaviour on contention |
//! |------------------------|---------------------------|-----------------------------------------|
//! | `None` (default) | (no tail) | ordinary SELECT, no lock |
//! | `ForUpdate` | `FOR UPDATE` | exclusive; blocks until released |
//! | `ForUpdateNowait` | `FOR UPDATE NOWAIT` | exclusive; errors with SQLSTATE 55P03 |
//! | `ForUpdateSkipLocked` | `FOR UPDATE SKIP LOCKED` | exclusive; silently skips locked rows |
//! | `ForShare` | `FOR SHARE` | shared; blocks writers, allows readers |
//! | `ForShareNowait` | `FOR SHARE NOWAIT` | shared; errors with SQLSTATE 55P03 |
//! | `ForShareSkipLocked` | `FOR SHARE SKIP LOCKED` | shared; silently skips locked rows |
//! `FOR SHARE` acquires a non-exclusive lock that blocks `UPDATE` /
//! `DELETE` of the same row but allows other readers to take the same
//! shared lock concurrently. Use it when multiple sessions need to
//! read-and-decide against the same row without any one of them
//! intending to write (e.g., two services checking a balance before
//! routing a transfer). For exclusive locking (read-then-write within
//! the same transaction), use the FOR UPDATE family.
//! # Pool-backed locks are a footgun — surface it loudly
//! Every row-level lock — `FOR UPDATE` or `FOR SHARE` — is held until
//! the end of the enclosing transaction. A pool-backed
//! [`DjogiContext`](crate::DjogiContext) auto-commits each statement,
//! so a `SELECT... FOR UPDATE` (or `... FOR SHARE`) on a pool-backed
//! context acquires the lock, then releases it instantly when the
//! implicit transaction closes — **no protection whatsoever** against
//! a concurrent writer between `fetch_*` and any follow-up step.
//! Terminal methods that execute the SELECT do NOT reject pool-backed
//! contexts when a non-`None` lock is set — that would be a runtime
//! error for a correctness question the caller can answer at the call
//! site. Instead, the rustdoc on every lock builder calls the
//! constraint out explicitly, and the integration suite pins the
//! `FOR UPDATE NOWAIT` semantic to an `atomic()` scope so the default
//! code path is correct. Callers who lock outside of `atomic()` are
//! expected to know what they are doing.

/// Row-level lock mode accumulated on a [`QuerySet`](crate::query::QuerySet).
/// Crate-private: users configure the lock via the typed builder
/// methods (`select_for_update` / `nowait` / `skip_locked` and the
/// FOR SHARE companions `select_for_share` / `for_share_nowait` /
/// `for_share_skip_locked`). The builders have last-call-wins
/// semantics — every modifier overwrites the prior variant outright.
/// Keeping the enum `pub(crate)` means downstream code cannot
/// hand-construct a variant that bypasses the `push_tail` emitter,
/// which is the only path that produces Postgres-valid lock SQL.
/// `Default` is `None` so a fresh `QuerySet<T>` carries no lock tail
/// the same behaviour shipped before landed.
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
    /// `FOR SHARE` — acquire a shared (non-exclusive) row lock for
    /// the duration of the enclosing transaction. Blocks `UPDATE` /
    /// `DELETE` of the locked rows but allows other readers to take
    /// the same `FOR SHARE` lock concurrently..
    ForShare,
    /// `FOR SHARE NOWAIT` — same shared-lock semantics as
    /// [`LockMode::ForShare`], but fail immediately with Postgres
    /// SQLSTATE `55P03` (`lock_not_available`) when another session
    /// holds a conflicting writer lock..
    ForShareNowait,
    /// `FOR SHARE SKIP LOCKED` — same shared-lock semantics as
    /// [`LockMode::ForShare`], but silently skip rows currently
    /// locked exclusively by another session..
    ForShareSkipLocked,
}

impl LockMode {
    /// Append the row-lock tail to the query builder, or no-op for
    /// [`LockMode::None`].
    /// Crate-private because only [`crate::query::sql::build_select`]
    /// and the joined-select variant emit row locks — terminals
    /// (`count`, `exists`, aggregates) never carry a lock because they
    /// don't return rows the caller can hold open against.
    pub(crate) fn push_tail(self, acc: &mut crate::pg::accumulator::SqlAccumulator) {
        match self {
            LockMode::None => {}
            LockMode::ForUpdate => {
                acc.push_sql(" FOR UPDATE");
            }
            LockMode::ForUpdateNowait => {
                acc.push_sql(" FOR UPDATE NOWAIT");
            }
            LockMode::ForUpdateSkipLocked => {
                acc.push_sql(" FOR UPDATE SKIP LOCKED");
            }
            LockMode::ForShare => {
                acc.push_sql(" FOR SHARE");
            }
            LockMode::ForShareNowait => {
                acc.push_sql(" FOR SHARE NOWAIT");
            }
            LockMode::ForShareSkipLocked => {
                acc.push_sql(" FOR SHARE SKIP LOCKED");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pg::accumulator::SqlAccumulator;

    #[test]
    fn none_emits_no_tail() {
        let mut acc = SqlAccumulator::new("");
        LockMode::None.push_tail(&mut acc);
        assert_eq!(acc.sql(), "");
    }

    #[test]
    fn for_update_emits_bare_clause() {
        let mut acc = SqlAccumulator::new("");
        LockMode::ForUpdate.push_tail(&mut acc);
        assert_eq!(acc.sql().trim(), "FOR UPDATE");
    }

    #[test]
    fn for_update_nowait_emits_nowait() {
        let mut acc = SqlAccumulator::new("");
        LockMode::ForUpdateNowait.push_tail(&mut acc);
        assert_eq!(acc.sql().trim(), "FOR UPDATE NOWAIT");
    }

    #[test]
    fn for_update_skip_locked_emits_skip_locked() {
        let mut acc = SqlAccumulator::new("");
        LockMode::ForUpdateSkipLocked.push_tail(&mut acc);
        assert_eq!(acc.sql().trim(), "FOR UPDATE SKIP LOCKED");
    }

    // ── FOR SHARE family ─────────────────────────────────

    #[test]
    fn for_share_emits_bare_clause() {
        let mut acc = SqlAccumulator::new("");
        LockMode::ForShare.push_tail(&mut acc);
        assert_eq!(acc.sql().trim(), "FOR SHARE");
    }

    #[test]
    fn for_share_nowait_emits_nowait() {
        let mut acc = SqlAccumulator::new("");
        LockMode::ForShareNowait.push_tail(&mut acc);
        assert_eq!(acc.sql().trim(), "FOR SHARE NOWAIT");
    }

    #[test]
    fn for_share_skip_locked_emits_skip_locked() {
        let mut acc = SqlAccumulator::new("");
        LockMode::ForShareSkipLocked.push_tail(&mut acc);
        assert_eq!(acc.sql().trim(), "FOR SHARE SKIP LOCKED");
    }

    #[test]
    fn for_share_tails_use_distinct_keywords_from_for_update() {
        // Regression guard: each FOR SHARE variant emits the FOR SHARE
        // keyword, never FOR UPDATE. A copy-paste slip on the
        // `push_tail` arm would surface here before any integration
        // test exercises the SQL emitter end-to-end.
        for (mode, expected) in [
            (LockMode::ForShare, "FOR SHARE"),
            (LockMode::ForShareNowait, "FOR SHARE NOWAIT"),
            (LockMode::ForShareSkipLocked, "FOR SHARE SKIP LOCKED"),
        ] {
            let mut acc = SqlAccumulator::new("");
            mode.push_tail(&mut acc);
            let sql = acc.sql().trim().to_owned();
            assert_eq!(sql, expected, "wrong tail for {mode:?}");
            assert!(
                !sql.contains("FOR UPDATE"),
                "FOR SHARE tail must not contain FOR UPDATE — got {sql:?} for {mode:?}"
            );
        }
    }

    #[test]
    fn default_is_none() {
        assert_eq!(LockMode::default(), LockMode::None);
    }
}
