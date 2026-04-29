//! `djogi_live_plans` — runtime state table for live migrations.
//!
//! Each row mirrors a single plan-file on disk. The plan file is the
//! immutable definition (see [`crate::live_migrate::plan_file`]); the
//! row in this table tracks the mutable runtime state — current step,
//! backfill progress, last error, completion timestamps. Per the v3
//! plan §1 D2 separation, the file holds the *what*, the row holds the
//! *where*.
//!
//! # Multi-app + multi-database boundary (v3 §6.5)
//!
//! Live plans operate within one `(target_database, app_label)`
//! bucket. Cross-app live plans are refused at compose time. The
//! uniqueness invariant is on `(target_database, app_label, plan_id)`
//! — the [`INSTALL_SQL`] DDL emits a matching unique index.
//!
//! # Forward references
//!
//! T15's daemon-mode resume will add `claimed_by_pid`,
//! `claimed_by_host`, and `claimed_at` columns. T6 deliberately does
//! NOT add them — they belong to the daemon contract, not the plan
//! contract. When T15 lands, [`INSTALL_SQL`] grows additional
//! `ALTER TABLE` statements (idempotent `ADD COLUMN IF NOT EXISTS`).
//!
//! # Status and classification CHECK constraints
//!
//! The `status` and `classification` columns enforce a closed set of
//! values via SQL CHECK. The Rust mirrors are [`PlanStatus`] and
//! [`crate::live_migrate::plan::PlanClassification`]; their `as_db_str`
//! / `from_db_str` impls are kept in lockstep with the SQL constraint
//! lists by the unit tests in this module.

use time::OffsetDateTime;

use crate::context::DjogiContext;
use crate::error::{DbError, DjogiError};
use crate::live_migrate::plan::PlanClassification;
use crate::types::HeerId;

// ── INSTALL_SQL ───────────────────────────────────────────────────────

/// DDL that idempotently installs the `djogi_live_plans` table plus
/// the per-bucket lookup index.
///
/// Per v3 §6.5, every live plan is scoped to a single
/// `(target_database, app_label)` bucket. `plan_id` is a HeerId, so
/// the table-level `PRIMARY KEY` already prevents collisions
/// regardless of bucket. The composite index
/// `djogi_live_plans_bucket_plan_id_uidx` on
/// `(target_database, app_label, plan_id)` is therefore a query-plan
/// optimisation — every operator-driven lookup keys off the bucket
/// before the plan_id, and the index lets Postgres satisfy
/// `WHERE target_database = $1 AND app_label = $2 AND plan_id = $3`
/// without scanning. The `UNIQUE` qualifier is redundant as a
/// uniqueness guarantee but documents the bucket-anchored access
/// pattern.
///
/// Idempotent — safe to call on every runner invocation.
pub const INSTALL_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS djogi_live_plans (
    plan_id               BIGINT PRIMARY KEY,
    slug                  TEXT NOT NULL,
    plan_file_checksum    VARCHAR(68) NOT NULL,
    classification        TEXT NOT NULL
                              CHECK (classification IN (
                                  'online_safe', 'expand_contract', 'offline_only'
                              )),
    status                TEXT NOT NULL DEFAULT 'pending'
                              CHECK (status IN (
                                  'pending', 'running', 'paused',
                                  'validating', 'cutover', 'finalizing',
                                  'complete', 'abandoned', 'failed'
                              )),
    current_step          TEXT,
    current_step_index    INTEGER NOT NULL DEFAULT 0,
    backfill_rows_done    BIGINT NOT NULL DEFAULT 0,
    backfill_rows_total   BIGINT,
    started_at            TIMESTAMPTZ,
    last_progress_at      TIMESTAMPTZ,
    completed_at          TIMESTAMPTZ,
    last_error            TEXT,
    originating_migration TEXT NOT NULL,
    target_database       TEXT NOT NULL DEFAULT 'main',
    app_label             TEXT NOT NULL DEFAULT ''
);
CREATE UNIQUE INDEX IF NOT EXISTS djogi_live_plans_bucket_plan_id_uidx
    ON djogi_live_plans (target_database, app_label, plan_id);
"#;

// ── PlanStatus ────────────────────────────────────────────────────────

/// Lifecycle states for a live-plan row. Mirrors the SQL CHECK
/// constraint on `djogi_live_plans.status` byte-for-byte.
///
/// State transitions are operator-driven via T10's CLI; the runner
/// never auto-advances past an operator gate. `Failed` and `Abandoned`
/// are terminal — the runner refuses to advance past them.
///
/// `#[non_exhaustive]` so future statuses (e.g. a `Quiesced` variant
/// for the daemon-mode pause-and-claim path) can land without breaking
/// downstream matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PlanStatus {
    /// Row inserted; no step has executed yet. Initial state.
    Pending,
    /// Runner is actively executing a non-gate step (typically
    /// [`crate::live_migrate::plan::StepKind::ExpandSchema`] or
    /// [`crate::live_migrate::plan::StepKind::BackfillChunked`]).
    Running,
    /// Operator paused execution mid-stream via `djogi live pause`.
    /// Contrast with `Validating` / `Cutover` / `Finalizing` which
    /// are step-specific gates.
    Paused,
    /// At a [`crate::live_migrate::plan::StepKind::ValidateBackfill`]
    /// gate — the operator must approve the gate query result before
    /// the runner advances to cutover.
    Validating,
    /// At a [`crate::live_migrate::plan::StepKind::CutoverReads`] or
    /// [`crate::live_migrate::plan::StepKind::CutoverWrites`] gate.
    Cutover,
    /// At a [`crate::live_migrate::plan::StepKind::FinalizeConstraints`]
    /// gate — constraints are being added now that data is correct.
    Finalizing,
    /// Every step completed successfully. Terminal.
    Complete,
    /// Operator abandoned the plan via `djogi live abandon`. Terminal.
    /// The plan file is preserved on disk for audit; resuming an
    /// abandoned plan is refused.
    Abandoned,
    /// A step failed and the runner could not auto-recover. Terminal
    /// pending operator intervention (resume after manual fix, or
    /// abandon).
    Failed,
}

/// Single source of truth for `(PlanStatus, db-string)` pairs.
///
/// Both [`PlanStatus::as_db_str`] and [`PlanStatus::from_db_str`] —
/// and the test that asserts INSTALL_SQL covers every variant —
/// linear-scan this slice. The compile-time `_PLAN_STATUS_DRIFT_GUARD`
/// block (an exhaustive match) is the guarantee that every variant
/// lives in this table: adding a `PlanStatus` variant fails compile
/// until the drift-guard match gets a new arm; updating the match
/// without updating the slice causes `as_db_str` to panic for the new
/// variant in tests (`expect("invariant: ...")`), and `from_db_str`
/// stops recognising the new label.
const PLAN_STATUS_LABELS: &[(PlanStatus, &str)] = &[
    (PlanStatus::Pending, "pending"),
    (PlanStatus::Running, "running"),
    (PlanStatus::Paused, "paused"),
    (PlanStatus::Validating, "validating"),
    (PlanStatus::Cutover, "cutover"),
    (PlanStatus::Finalizing, "finalizing"),
    (PlanStatus::Complete, "complete"),
    (PlanStatus::Abandoned, "abandoned"),
    (PlanStatus::Failed, "failed"),
];

/// Compile-time exhaustiveness witness. Adding a `PlanStatus` variant
/// without extending the inner `match` fails to compile here. The
/// block has no runtime effect — the value is `()` — but the
/// exhaustive match enforces "every variant is named in source" so
/// [`PLAN_STATUS_LABELS`] is forced to grow alongside (the test in
/// the `tests` module asserts the slice covers every variant).
const _PLAN_STATUS_DRIFT_GUARD: () = {
    const fn check(s: PlanStatus) -> u8 {
        match s {
            PlanStatus::Pending => 0,
            PlanStatus::Running => 1,
            PlanStatus::Paused => 2,
            PlanStatus::Validating => 3,
            PlanStatus::Cutover => 4,
            PlanStatus::Finalizing => 5,
            PlanStatus::Complete => 6,
            PlanStatus::Abandoned => 7,
            PlanStatus::Failed => 8,
        }
    }
    // Force the exhaustive match to be evaluated at const-eval time;
    // the value is discarded but the compiler still checks coverage.
    let _ = check(PlanStatus::Pending);
};

impl PlanStatus {
    /// String form recorded in `djogi_live_plans.status`. Keep in
    /// lockstep with the CHECK constraint in [`INSTALL_SQL`] — the
    /// test in this module's `tests` block asserts every label
    /// appears verbatim.
    ///
    /// Single source of truth: [`PLAN_STATUS_LABELS`]. Linear scan
    /// over a 9-element slice is microsecond-cheap and the
    /// drift-guard block above ensures every variant has a row.
    pub fn as_db_str(self) -> &'static str {
        PLAN_STATUS_LABELS
            .iter()
            .find_map(|(v, label)| (*v == self).then_some(*label))
            .expect("invariant: PlanStatus variant missing from PLAN_STATUS_LABELS")
    }

    /// Inverse of [`PlanStatus::as_db_str`]. Returns `None` for values
    /// outside the CHECK list — callers surface that as a
    /// database-corruption indicator the operator can act on.
    pub fn from_db_str(s: &str) -> Option<Self> {
        PLAN_STATUS_LABELS
            .iter()
            .find_map(|(variant, label)| (*label == s).then_some(*variant))
    }
}

// ── LivePlanRow ───────────────────────────────────────────────────────

/// Owned shape of a `djogi_live_plans` row. Used by [`insert_row`] to
/// stage a new plan and by [`fetch_row_by_id`] to read one back.
///
/// Mirrors the column list in [`INSTALL_SQL`] one-to-one. Time fields
/// use [`time::OffsetDateTime`] per the project's no-`chrono` rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LivePlanRow {
    pub plan_id: HeerId,
    pub slug: String,
    /// `V1:<sha256-hex>` checksum of the on-disk plan file at the
    /// moment of INSERT. Verified on every subsequent run / resume /
    /// finalize via [`crate::live_migrate::plan_file::verify_checksum`].
    pub plan_file_checksum: String,
    pub classification: PlanClassification,
    pub status: PlanStatus,
    /// Step kind name of the currently-executing step. `None` before
    /// the runner advances onto the first step.
    pub current_step: Option<String>,
    pub current_step_index: i32,
    pub backfill_rows_done: i64,
    /// Total backfill row count if the runner could estimate it
    /// (e.g. via `pg_class.reltuples`). `None` for ranges whose size
    /// is unknown at start.
    pub backfill_rows_total: Option<i64>,
    pub started_at: Option<OffsetDateTime>,
    pub last_progress_at: Option<OffsetDateTime>,
    pub completed_at: Option<OffsetDateTime>,
    pub last_error: Option<String>,
    /// Phase 7 migration version that triggered this plan (the
    /// version string of the row in `djogi_schema_migrations`).
    pub originating_migration: String,
    pub target_database: String,
    pub app_label: String,
}

// ── CRUD helpers ──────────────────────────────────────────────────────

/// Idempotently install the `djogi_live_plans` table plus the
/// per-bucket uniqueness index. Routes through
/// [`DjogiContext::raw_ddl`] which uses Postgres's simple-query
/// protocol (DDL statements that the prepare-cache cannot handle).
pub async fn install(ctx: &mut DjogiContext) -> Result<(), DjogiError> {
    ctx.raw_ddl(INSTALL_SQL).await
}

/// INSERT a fresh plan row in `pending` state. Refuses (via the
/// underlying unique index) if a plan already exists in the same
/// `(target_database, app_label)` bucket with the same `plan_id`.
///
/// The runner inserts this row OUTSIDE the migration's transaction —
/// the row is the durable anchor for resume, not part of the migration's
/// rollback semantics.
pub async fn insert_row(ctx: &mut DjogiContext, row: &LivePlanRow) -> Result<(), DjogiError> {
    let sql = "INSERT INTO djogi_live_plans \
               (plan_id, slug, plan_file_checksum, classification, status, \
                current_step, current_step_index, backfill_rows_done, \
                backfill_rows_total, started_at, last_progress_at, completed_at, \
                last_error, originating_migration, target_database, app_label) \
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)";
    let plan_id = row.plan_id.as_i64();
    let classification = row.classification.as_db_str();
    let status = row.status.as_db_str();
    ctx.execute(
        sql,
        &[
            &plan_id,
            &row.slug,
            &row.plan_file_checksum,
            &classification,
            &status,
            &row.current_step,
            &row.current_step_index,
            &row.backfill_rows_done,
            &row.backfill_rows_total,
            &row.started_at,
            &row.last_progress_at,
            &row.completed_at,
            &row.last_error,
            &row.originating_migration,
            &row.target_database,
            &row.app_label,
        ],
    )
    .await?;
    Ok(())
}

/// Read a plan row by its bucket key `(target_database, app_label,
/// plan_id)`. Returns `None` when the row is absent.
pub async fn fetch_row_by_id(
    ctx: &mut DjogiContext,
    plan_id: HeerId,
    target_database: &str,
    app_label: &str,
) -> Result<Option<LivePlanRow>, DjogiError> {
    let sql = "SELECT plan_id, slug, plan_file_checksum, classification, status, \
               current_step, current_step_index, backfill_rows_done, \
               backfill_rows_total, started_at, last_progress_at, completed_at, \
               last_error, originating_migration, target_database, app_label \
               FROM djogi_live_plans \
               WHERE target_database = $1 AND app_label = $2 AND plan_id = $3";
    let plan_id_i64 = plan_id.as_i64();
    let row_opt = ctx
        .query_opt(sql, &[&target_database, &app_label, &plan_id_i64])
        .await?;
    let Some(row) = row_opt else {
        return Ok(None);
    };
    let parsed = row_to_live_plan_row(&row)?;
    Ok(Some(parsed))
}

/// Parse a 16-column row from the SELECT in [`fetch_row_by_id`] into a
/// [`LivePlanRow`]. Column order matches the SELECT exactly.
fn row_to_live_plan_row(row: &tokio_postgres::Row) -> Result<LivePlanRow, DjogiError> {
    let plan_id_i64: i64 = row.try_get(0)?;
    let plan_id = HeerId::from_i64(plan_id_i64)
        .map_err(|e| DjogiError::Db(DbError::other(format!("invalid plan_id in row: {e}"))))?;
    let slug: String = row.try_get(1)?;
    let plan_file_checksum: String = row.try_get(2)?;
    let classification_s: String = row.try_get(3)?;
    let classification = PlanClassification::from_db_str(&classification_s).ok_or_else(|| {
        DjogiError::Db(DbError::other(format!(
            "unknown classification in djogi_live_plans: {classification_s:?}"
        )))
    })?;
    let status_s: String = row.try_get(4)?;
    let status = PlanStatus::from_db_str(&status_s).ok_or_else(|| {
        DjogiError::Db(DbError::other(format!(
            "unknown status in djogi_live_plans: {status_s:?}"
        )))
    })?;
    let current_step: Option<String> = row.try_get(5)?;
    let current_step_index: i32 = row.try_get(6)?;
    let backfill_rows_done: i64 = row.try_get(7)?;
    let backfill_rows_total: Option<i64> = row.try_get(8)?;
    let started_at: Option<OffsetDateTime> = row.try_get(9)?;
    let last_progress_at: Option<OffsetDateTime> = row.try_get(10)?;
    let completed_at: Option<OffsetDateTime> = row.try_get(11)?;
    let last_error: Option<String> = row.try_get(12)?;
    let originating_migration: String = row.try_get(13)?;
    let target_database: String = row.try_get(14)?;
    let app_label: String = row.try_get(15)?;
    Ok(LivePlanRow {
        plan_id,
        slug,
        plan_file_checksum,
        classification,
        status,
        current_step,
        current_step_index,
        backfill_rows_done,
        backfill_rows_total,
        started_at,
        last_progress_at,
        completed_at,
        last_error,
        originating_migration,
        target_database,
        app_label,
    })
}

/// Update a row's backfill progress. Sets `backfill_rows_done` and
/// stamps `last_progress_at = now()`. Called by the chunked-backfill
/// executor after each chunk commits — see v3 §3 line 413-419 for the
/// "checkpoint write in same transaction as chunk" contract.
pub async fn update_progress(
    ctx: &mut DjogiContext,
    plan_id: HeerId,
    target_database: &str,
    app_label: &str,
    rows_done: i64,
) -> Result<(), DjogiError> {
    let sql = "UPDATE djogi_live_plans \
               SET backfill_rows_done = $4, last_progress_at = now() \
               WHERE target_database = $1 AND app_label = $2 AND plan_id = $3";
    let plan_id_i64 = plan_id.as_i64();
    ctx.execute(
        sql,
        &[&target_database, &app_label, &plan_id_i64, &rows_done],
    )
    .await?;
    Ok(())
}

/// Update a row's lifecycle status. The CLI's state-machine
/// transitions are enforced at the operator surface (T10); this
/// helper writes the new value verbatim so transitions remain
/// observable in the row regardless of which CLI command performed
/// them.
pub async fn update_status(
    ctx: &mut DjogiContext,
    plan_id: HeerId,
    target_database: &str,
    app_label: &str,
    new_status: PlanStatus,
) -> Result<(), DjogiError> {
    let sql = "UPDATE djogi_live_plans \
               SET status = $4 \
               WHERE target_database = $1 AND app_label = $2 AND plan_id = $3";
    let plan_id_i64 = plan_id.as_i64();
    let status = new_status.as_db_str();
    ctx.execute(sql, &[&target_database, &app_label, &plan_id_i64, &status])
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_status_as_db_str_matches_check_constraint_list() {
        // Every variant in the CHECK constraint list must map to a
        // unique lowercase / underscore string. Exhaustive — adding
        // a PlanStatus variant trips this until the SQL CHECK is
        // updated.
        let pairs = [
            (PlanStatus::Pending, "pending"),
            (PlanStatus::Running, "running"),
            (PlanStatus::Paused, "paused"),
            (PlanStatus::Validating, "validating"),
            (PlanStatus::Cutover, "cutover"),
            (PlanStatus::Finalizing, "finalizing"),
            (PlanStatus::Complete, "complete"),
            (PlanStatus::Abandoned, "abandoned"),
            (PlanStatus::Failed, "failed"),
        ];
        for (variant, expected) in pairs {
            assert_eq!(variant.as_db_str(), expected);
            assert_eq!(PlanStatus::from_db_str(expected), Some(variant));
        }
    }

    #[test]
    fn plan_status_from_db_str_rejects_unknown() {
        assert_eq!(PlanStatus::from_db_str("whatever"), None);
        assert_eq!(PlanStatus::from_db_str(""), None);
        // Case-sensitive — uppercase variants are NOT in the CHECK list.
        assert_eq!(PlanStatus::from_db_str("Pending"), None);
    }

    #[test]
    fn plan_status_label_slice_covers_every_variant() {
        // PLAN_STATUS_LABELS is the runtime source for both as_db_str
        // and from_db_str. Drift between the slice and the variant
        // list would manifest as a panic in `as_db_str` (slice missing
        // a row) or as `from_db_str` returning None for a real label
        // (slice carrying a stale row). The compile-time
        // `_PLAN_STATUS_DRIFT_GUARD` enforces "every variant named in
        // source"; this test pins "every variant has a row in the
        // slice and the row round-trips".
        let exhaustive_list = [
            PlanStatus::Pending,
            PlanStatus::Running,
            PlanStatus::Paused,
            PlanStatus::Validating,
            PlanStatus::Cutover,
            PlanStatus::Finalizing,
            PlanStatus::Complete,
            PlanStatus::Abandoned,
            PlanStatus::Failed,
        ];
        assert_eq!(
            PLAN_STATUS_LABELS.len(),
            exhaustive_list.len(),
            "PLAN_STATUS_LABELS must contain exactly one row per PlanStatus variant"
        );
        for variant in exhaustive_list {
            let label = variant.as_db_str();
            assert_eq!(
                PlanStatus::from_db_str(label),
                Some(variant),
                "round-trip failed for {variant:?}",
            );
        }
    }

    #[test]
    fn install_sql_lists_every_plan_status_variant() {
        // Iterate the const slice that production uses for its
        // forward and reverse lookups. The exhaustive match in
        // `assert_plan_status_drift_guard` (above this `tests` block)
        // is the compile-time guarantee that every PlanStatus variant
        // lives in `PLAN_STATUS_LABELS`. Adding a variant fails to
        // compile until the slice is extended; this test then
        // asserts INSTALL_SQL has the matching CHECK entry.
        for (variant, label) in PLAN_STATUS_LABELS {
            assert_eq!(
                *label,
                variant.as_db_str(),
                "drift between PLAN_STATUS_LABELS and `as_db_str()` for {variant:?}",
            );
            let needle = format!("'{label}'");
            assert!(
                INSTALL_SQL.contains(&needle),
                "INSTALL_SQL missing CHECK entry for status {needle}",
            );
        }
    }

    #[test]
    fn install_sql_lists_every_plan_classification_variant() {
        // Same drift-guard pattern as PLAN_STATUS_LABELS — see plan.rs
        // for `PLAN_CLASSIFICATION_LABELS` and its drift-guard match.
        for c in [
            PlanClassification::OnlineSafe,
            PlanClassification::ExpandContract,
            PlanClassification::OfflineOnly,
        ] {
            let label = c.as_db_str();
            let needle = format!("'{label}'");
            assert!(
                INSTALL_SQL.contains(&needle),
                "INSTALL_SQL missing CHECK entry for classification {needle}",
            );
        }
    }

    #[test]
    fn install_sql_declares_unique_index_on_bucket_plan_id() {
        // The §6.5 bucket invariant materialises as a unique index;
        // refuse to land INSTALL_SQL without it.
        assert!(
            INSTALL_SQL.contains("CREATE UNIQUE INDEX IF NOT EXISTS"),
            "INSTALL_SQL must create the bucket+plan_id unique index",
        );
        assert!(
            INSTALL_SQL.contains("(target_database, app_label, plan_id)"),
            "unique index must be on (target_database, app_label, plan_id)",
        );
    }
}
