//! Partition / vacuum analysis for adopter Postgres tables — pure
//! substrate layer.
//!
//! `djogi analyze` (T10) inspects `pg_stat_user_tables` and (when the
//! extension is installed) `pg_partman` metadata to surface vacuum and
//! partitioning recommendations to operators. This module ships the
//! **pure** recommendation logic — no DB, no I/O, no global state. The
//! live-DB query path (`fetch_table_health`) and CLI dispatch land in
//! T10.2; the integration test lands in T10.3.
//!
//! # Why a pure substrate
//!
//! `recommend()` is exposed as a free function taking only
//! `&TableHealth` plus scalar threshold args. That shape is
//! deliberately deterministic — the same inputs always produce the
//! exact same output bytes. Two consequences fall out:
//!
//! 1. **Byte-stable JSON.** When T10.2 serialises a sorted
//!    `Vec<(table_name, Recommendation)>` to `serde_json`, the result
//!    is reproducible across runs / hosts / Postgres restarts. CI
//!    dashboards that diff yesterday's `analyze --format json` output
//!    against today's see only real changes, never iteration-order
//!    churn.
//! 2. **Trivial unit-testability.** No `tokio` runtime, no fixture DB,
//!    no temp dirs — every recommendation rule is exercised in-process
//!    against hand-built `TableHealth` values.
//!
//! # Threshold rationale
//!
//! Both thresholds are runtime arguments rather than constants because
//! healthy bloat / partition-row ceilings vary per workload. The
//! defaults chosen by the CLI (`0.2` and `10_000_000`) are conservative
//! middle-of-the-road values; OLTP-heavy tables typically tighten the
//! vacuum threshold, while warehouse-style tables loosen the partition
//! row count. Adopters override on the command line without recompiling.
//!
//! # Spec
//!
//! `docs/superpowers/plans/granular-phase8/cluster-8epsilon-granular.md`
//! §T10.1.

// T10.1 ships the substrate only — the public surface is exercised by
// the in-module unit tests but is not yet referenced from `main.rs`'s
// dispatch arm (the placeholder `eprintln!` lands first; T10.2 swaps
// in the real call). Until then the items would trigger `dead_code`
// under `-D warnings`. The allow disappears in T10.2 along with the
// placeholder.
#![allow(dead_code)]

use serde::Serialize;

/// Snapshot of a single table's vacuum / partition health.
///
/// Field provenance (per T10.2's planned query):
///
/// - `table_name` — `pg_stat_user_tables.relname`
/// - `n_live_tup`, `n_dead_tup` — `pg_stat_user_tables` columns of the
///   same name; Postgres-maintained per-row visibility counters.
/// - `last_analyze` — `pg_stat_user_tables.last_analyze`; `None` when
///   the table has never been analysed (e.g. freshly created).
/// - `partition_count` — `0` for plain tables, `>= 1` for partitioned
///   parents (sourced via `pg_partitioned_table` join, with a
///   `pg_partman` fallback when the extension is installed).
///
/// `last_analyze` is intentionally `time::OffsetDateTime`, not
/// `chrono::DateTime` — djogi forbids `chrono` workspace-wide
/// (CLAUDE.md "Dependencies excluded").
#[derive(Debug, Clone, Serialize)]
pub struct TableHealth {
    pub table_name: String,
    pub n_live_tup: i64,
    pub n_dead_tup: i64,
    pub last_analyze: Option<time::OffsetDateTime>,
    pub partition_count: i32,
}

/// Recommendation produced by [`recommend`] for a single table.
///
/// # Precedence
///
/// When multiple rules would fire, [`recommend`] returns the
/// highest-priority match per this strict ordering (highest first):
///
/// 1. [`Recommendation::VacuumNeeded`] — bloat dominates everything;
///    autovacuum lag is the most operationally urgent signal because
///    dead tuples block index health and inflate disk usage.
/// 2. [`Recommendation::PartitionRecommended`] — an unpartitioned table
///    has crossed the row-count threshold; partitioning is structural
///    work that should land before the table grows further.
/// 3. [`Recommendation::PartitionCountIncrease`] — partitions exist
///    but average row count per partition exceeds the threshold;
///    expanding the partition count is incremental tuning.
/// 4. [`Recommendation::Healthy`] — no rule fires.
///
/// # JSON shape
///
/// The `#[serde(tag = "kind", rename_all = "snake_case")]` attribute
/// produces internally-tagged JSON like
/// `{"kind":"vacuum_needed","dead_tup_ratio":0.42}`. T10.2's
/// `--format json` path serialises a sorted vector of
/// `{table, recommendation}` pairs; the snake_case tag keeps the
/// machine-readable output ergonomic for shell scripts and dashboards.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Recommendation {
    /// Dead-tuple ratio exceeded `threshold_vacuum`; operator should
    /// run `VACUUM` (or tune autovacuum).
    VacuumNeeded {
        /// `n_dead_tup / (n_live_tup + n_dead_tup)`. Always strictly
        /// greater than the threshold that triggered the variant.
        dead_tup_ratio: f64,
    },
    /// Unpartitioned table whose live row count exceeds
    /// `threshold_partition_rows`; operator should partition.
    PartitionRecommended {
        /// Human-readable explanation including the row count and the
        /// threshold that fired the rule. Stable string format so
        /// `--format json` consumers can grep for substrings (`"not
        /// partitioned"`, `"threshold:"`).
        reason: String,
    },
    /// Partitioned table whose average rows-per-partition exceeds
    /// `threshold_partition_rows`; operator should expand the partition
    /// count.
    PartitionCountIncrease {
        /// Current partition count.
        current: i32,
        /// Suggested partition count — currently a simple doubling.
        /// Bounded by `i32::saturating_mul` so pathological inputs
        /// (e.g. 1.5B partitions) cap at `i32::MAX` rather than
        /// overflowing.
        suggested: i32,
    },
    /// No recommendation — table is within all thresholds.
    Healthy,
}

/// Pure recommendation function for a single table.
///
/// # Determinism
///
/// `recommend` takes only the borrowed `TableHealth` and two scalar
/// thresholds. It performs no I/O, reads no globals, allocates only
/// the `String` inside `PartitionRecommended::reason` (when that arm
/// fires), and traverses no unordered collections. Repeated invocation
/// on byte-identical inputs returns byte-identical outputs — the
/// `recommend_is_deterministic` test asserts this with 100 repetitions.
///
/// # Threshold semantics
///
/// - `threshold_vacuum`: dead-tuple ratio strictly above which
///   [`Recommendation::VacuumNeeded`] fires. Typical: `0.2` (20% bloat).
///   Higher values mean the operator tolerates more bloat before
///   flagging.
/// - `threshold_partition_rows`: live row count strictly above which
///   an unpartitioned table triggers [`Recommendation::PartitionRecommended`].
///   Typical: `10_000_000`. The same threshold is reused for the
///   per-partition row average that drives
///   [`Recommendation::PartitionCountIncrease`].
///
/// # Edge cases
///
/// - Empty table (`n_live_tup == 0 && n_dead_tup == 0`): vacuum check
///   is short-circuited (division-by-zero guard). Partition checks
///   still run but neither fires for an empty table.
/// - `partition_count == 0`: treated as "not partitioned" — only the
///   `PartitionRecommended` rule can fire.
/// - `partition_count >= 1` but row count below threshold: falls
///   through to `Healthy`.
///
/// # See also
///
/// [`Recommendation`] for the precedence ordering.
pub fn recommend(
    health: &TableHealth,
    threshold_vacuum: f64,
    threshold_partition_rows: i64,
) -> Recommendation {
    // 1. VacuumNeeded — highest priority. Skipped on empty tables to
    //    avoid 0/0; an empty table cannot be bloated by definition.
    //    `saturating_add` caps at `i64::MAX` rather than panicking
    //    (debug) or wrapping (release) when both counters approach
    //    `i64::MAX` — pathological stats values still produce a valid
    //    ratio in `[0.0, 1.0]`.
    let total_tup = health.n_live_tup.saturating_add(health.n_dead_tup);
    if total_tup > 0 {
        let ratio = health.n_dead_tup as f64 / total_tup as f64;
        if ratio > threshold_vacuum {
            return Recommendation::VacuumNeeded {
                dead_tup_ratio: ratio,
            };
        }
    }

    // 2. PartitionRecommended — unpartitioned table over the row
    //    threshold. `partition_count == 0` is the unpartitioned signal.
    if health.partition_count == 0 && health.n_live_tup > threshold_partition_rows {
        return Recommendation::PartitionRecommended {
            reason: format!(
                "table has {} live rows but is not partitioned (threshold: {})",
                health.n_live_tup, threshold_partition_rows
            ),
        };
    }

    // 3. PartitionCountIncrease — partitioned but undersized partitions.
    //    Average is integer-divided; precision is irrelevant since the
    //    threshold gate is also an integer comparison.
    if health.partition_count > 0 {
        let avg_per_partition = health.n_live_tup / health.partition_count as i64;
        if avg_per_partition > threshold_partition_rows {
            return Recommendation::PartitionCountIncrease {
                current: health.partition_count,
                // Saturating multiplication caps at `i32::MAX` rather
                // than overflowing — pathological partition counts
                // (e.g. > i32::MAX/2) still produce a valid suggestion.
                suggested: health.partition_count.saturating_mul(2),
            };
        }
    }

    // 4. Healthy — no rule fired.
    Recommendation::Healthy
}

#[cfg(test)]
mod tests {
    //! Pure unit tests covering every arm of `Recommendation` plus
    //! precedence ordering and determinism. None of these tests touch
    //! the network, the filesystem, or a database — they construct
    //! `TableHealth` values directly and assert on the returned
    //! `Recommendation`.

    use super::*;

    /// Helper: build a `TableHealth` with sensible defaults so tests
    /// only override the fields they care about. Centralising the
    /// builder keeps test bodies focused on the rule being exercised.
    fn health(n_live_tup: i64, n_dead_tup: i64, partition_count: i32) -> TableHealth {
        TableHealth {
            table_name: "test_table".to_string(),
            n_live_tup,
            n_dead_tup,
            last_analyze: None,
            partition_count,
        }
    }

    #[test]
    fn recommend_healthy_when_below_all_thresholds() {
        // Small table, no dead tuples, plenty of partition headroom.
        let h = health(1_000, 0, 0);
        assert_eq!(recommend(&h, 0.2, 10_000_000), Recommendation::Healthy);

        // Same idea but partitioned and well below the per-partition
        // ceiling — also Healthy.
        let h = health(100_000, 0, 4);
        assert_eq!(recommend(&h, 0.2, 10_000_000), Recommendation::Healthy);

        // Genuinely empty table — Healthy (vacuum guard skips the
        // division, partition checks don't fire below threshold).
        let h = health(0, 0, 0);
        assert_eq!(recommend(&h, 0.2, 10_000_000), Recommendation::Healthy);
    }

    #[test]
    fn recommend_vacuum_when_dead_tup_ratio_high() {
        // Just above 0.2 — fires.
        // 21 dead / (79 live + 21 dead) = 0.21
        let h = health(79, 21, 0);
        match recommend(&h, 0.2, 10_000_000) {
            Recommendation::VacuumNeeded { dead_tup_ratio } => {
                assert!(
                    dead_tup_ratio > 0.20 && dead_tup_ratio < 0.22,
                    "expected ratio near 0.21, got {dead_tup_ratio}"
                );
            }
            other => panic!("expected VacuumNeeded, got {other:?}"),
        }

        // Just below 0.2 — does NOT fire.
        // 19 dead / 100 = 0.19
        let h = health(81, 19, 0);
        assert_eq!(recommend(&h, 0.2, 10_000_000), Recommendation::Healthy);

        // Exactly at 0.2 — does NOT fire (strict greater-than).
        let h = health(80, 20, 0);
        assert_eq!(recommend(&h, 0.2, 10_000_000), Recommendation::Healthy);

        // High ratio (50%) — fires unambiguously.
        let h = health(50, 50, 0);
        match recommend(&h, 0.2, 10_000_000) {
            Recommendation::VacuumNeeded { dead_tup_ratio } => {
                assert!((dead_tup_ratio - 0.5).abs() < 1e-9);
            }
            other => panic!("expected VacuumNeeded, got {other:?}"),
        }
    }

    #[test]
    fn recommend_partition_when_unpartitioned_and_large() {
        // Just above 10M rows, no dead tuples, no partitions.
        let h = health(10_000_001, 0, 0);
        match recommend(&h, 0.2, 10_000_000) {
            Recommendation::PartitionRecommended { reason } => {
                assert!(reason.contains("10000001"), "reason: {reason}");
                assert!(reason.contains("not partitioned"), "reason: {reason}");
                assert!(reason.contains("threshold: 10000000"), "reason: {reason}");
            }
            other => panic!("expected PartitionRecommended, got {other:?}"),
        }

        // Exactly at threshold — does NOT fire (strict greater-than).
        let h = health(10_000_000, 0, 0);
        assert_eq!(recommend(&h, 0.2, 10_000_000), Recommendation::Healthy);

        // Way over threshold but already partitioned — does NOT fire
        // the unpartitioned rule (CountIncrease may, see below).
        let h = health(20_000_000, 0, 100);
        // 20M / 100 = 200k average → below 10M threshold → Healthy.
        assert_eq!(recommend(&h, 0.2, 10_000_000), Recommendation::Healthy);
    }

    #[test]
    fn recommend_partition_count_increase_when_partitions_undersized() {
        // 100M rows across 4 partitions = 25M each → exceeds 10M.
        let h = health(100_000_000, 0, 4);
        assert_eq!(
            recommend(&h, 0.2, 10_000_000),
            Recommendation::PartitionCountIncrease {
                current: 4,
                suggested: 8,
            }
        );

        // Saturating-mul guard: even pathological partition counts
        // produce a valid `i32` suggestion.
        let h = health(i64::MAX / 2, 0, i32::MAX);
        match recommend(&h, 0.2, 10_000_000) {
            Recommendation::PartitionCountIncrease { current, suggested } => {
                assert_eq!(current, i32::MAX);
                assert_eq!(suggested, i32::MAX); // saturated
            }
            other => panic!("expected PartitionCountIncrease, got {other:?}"),
        }
    }

    #[test]
    fn recommend_is_deterministic() {
        // Build a single TableHealth and run recommend() 100 times;
        // every result must equal the first. Covers the v3 §494
        // concern about HashMap-iteration nondeterminism — there is no
        // HashMap in `recommend`, but the test cements the contract so
        // future refactors don't sneak one in.
        let h = health(50_000_000, 0, 3);
        let baseline = recommend(&h, 0.2, 10_000_000);

        for i in 0..100 {
            let result = recommend(&h, 0.2, 10_000_000);
            assert_eq!(
                result, baseline,
                "iteration {i} diverged from baseline {baseline:?}"
            );
        }

        // Same shape with the VacuumNeeded arm — float math should
        // also be bit-stable across repeated invocations on the same
        // inputs.
        let h = health(70, 30, 0);
        let baseline = recommend(&h, 0.2, 10_000_000);
        for i in 0..100 {
            assert_eq!(
                recommend(&h, 0.2, 10_000_000),
                baseline,
                "vacuum iteration {i} diverged"
            );
        }
    }

    #[test]
    fn recommend_vacuum_dominates_partition() {
        // Both VacuumNeeded AND PartitionRecommended would fire in
        // isolation — vacuum wins per precedence ordering.
        //
        // Setup: 100M live + 50M dead → 33% dead ratio AND
        // unpartitioned over the 10M row threshold.
        let h = health(100_000_000, 50_000_000, 0);
        match recommend(&h, 0.2, 10_000_000) {
            Recommendation::VacuumNeeded { dead_tup_ratio } => {
                assert!((dead_tup_ratio - (50.0 / 150.0)).abs() < 1e-9);
            }
            other => panic!("expected VacuumNeeded (precedence), got {other:?}"),
        }
    }

    #[test]
    fn recommend_partition_dominates_count_increase() {
        // An unpartitioned table cannot trigger CountIncrease at all
        // (CountIncrease requires `partition_count > 0`), so this test
        // pins the precedence boundary the other way: a partitioned
        // table that crosses both the size *and* the per-partition
        // ceiling falls into CountIncrease — there is no way for
        // PartitionRecommended to fire on a partitioned table by
        // construction.
        //
        // Rule still under test: when both partition rules *could*
        // logically apply, PartitionRecommended only matches the
        // unpartitioned case (`partition_count == 0`).
        let h = health(100_000_000, 0, 4); // partitioned, 25M/partition
        match recommend(&h, 0.2, 10_000_000) {
            Recommendation::PartitionCountIncrease { current, suggested } => {
                assert_eq!(current, 4);
                assert_eq!(suggested, 8);
            }
            other => panic!("expected PartitionCountIncrease, got {other:?}"),
        }

        // And the inverse: unpartitioned + over threshold goes to
        // PartitionRecommended, NOT CountIncrease.
        let h = health(100_000_000, 0, 0);
        assert!(matches!(
            recommend(&h, 0.2, 10_000_000),
            Recommendation::PartitionRecommended { .. }
        ));
    }

    #[test]
    fn recommend_handles_n_tup_addition_overflow() {
        // Both counters at `i64::MAX` would panic in debug or silently
        // wrap in release under unchecked addition. `saturating_add`
        // caps at `i64::MAX`, so the ratio is `i64::MAX / i64::MAX = 1.0`
        // — well above the default 0.2 threshold, so VacuumNeeded fires.
        // The test pins the contract: pathological stats values must NOT
        // panic and must still produce a deterministic recommendation.
        let h = TableHealth {
            table_name: "boom".to_string(),
            n_live_tup: i64::MAX,
            n_dead_tup: i64::MAX,
            last_analyze: None,
            partition_count: 0,
        };
        let result = recommend(&h, 0.2, 10_000_000);
        match result {
            Recommendation::VacuumNeeded { dead_tup_ratio } => {
                // i64::MAX / i64::MAX (saturated) = 1.0
                assert!(
                    (dead_tup_ratio - 1.0).abs() < 1e-9,
                    "expected ratio 1.0, got {dead_tup_ratio}"
                );
            }
            other => panic!("expected VacuumNeeded, got {other:?}"),
        }
    }
}
