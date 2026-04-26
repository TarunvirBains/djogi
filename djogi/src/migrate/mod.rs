//! Migration system — Phase 7's home for everything that takes
//! `ModelDescriptor` inventory and lowers it to executable Postgres
//! migrations.
//!
//! The module fans out to several concerns:
//!
//! | Submodule | Role |
//! |-----------|------|
//! | [`schema`] | Owned snapshot data types — what `schema_snapshot.json` round-trips through. |
//! | [`snapshot`] | Snapshot file I/O — `save_snapshot` / `load_snapshot` with `format_version` validation. |
//! | [`projection`] | Bridges static-lifetime descriptors → owned `AppliedSchema`. |
//! | [`diff`] | Schema differ — produces `SchemaDelta` from two `AppliedSchema`. |
//! | [`sql`] | Lowers `SchemaDelta` operations into reviewable up / down SQL pairs. |
//! | [`segment`] | Plans `SchemaDelta` into transactional / non-transactional / metadata-only segments. |
//! | [`guard`] | File-level workspace lock primitive used by `compose` / `attune` / `apply` / `repair` / `baseline`. |
//! | [`ledger`] | `djogi_schema_migrations` DDL bootstrap, row CRUD, and `V1:<sha256-hex>` checksum format. |
//! | [`runner`] | Apply orchestration — advisory lock, transactional / non-transactional segment dispatch, partial-state recording, snapshot persist on success. |
//!
//! T8 adds `docs`, `seed`, and `reset` — markdown documentation
//! generation, the seed runner / `djogi_seed_runs` ledger, and the
//! triple-gated `db reset` orchestrator.
//!
//! # Public surface
//!
//! Today the public entry points are:
//!
//! - [`AppliedSchema`] / [`TableSchema`] / [`ColumnSchema`] etc. —
//!   the snapshot data model.
//! - [`SNAPSHOT_FORMAT_VERSION`] — the current snapshot version
//!   string (loaders reject anything else).
//! - [`save_snapshot`] / [`load_snapshot`] / [`parse_snapshot_bytes`]
//!   / [`serialize_snapshot`] — file I/O helpers.
//! - [`SnapshotError`] — error variants surfaced by I/O paths.
//! - [`BucketKey`] — `(database, app)` identity that keys per-bucket
//!   snapshots.
//! - [`ProjectionError`] — error variants surfaced when projecting
//!   the descriptor inventory.
//! - [`project_from_inventory`] — production entry point; walks the
//!   global `inventory::iter` collectors and produces one
//!   [`AppliedSchema`] per [`BucketKey`].
//!
//! The lower-level [`projection::project_from_iters`] is `pub(crate)`
//! and exists for tests + the T10 `#[djogi_test(sync_models)]`
//! helper. External consumers use [`project_from_inventory`].
//!
//! Diff entry points: external consumers use [`diff_bucket_maps`]
//! which correctly handles cross-bucket moves. The per-bucket
//! `diff_schemas` is `pub(crate)` and only used by the bucket-walk
//! worker.
//!
//! SQL + segment entry points: external consumers use
//! [`plan_delta`] (typically) or [`lower_delta`] (when only the
//! per-operation SQL pairs are needed without segment grouping).
//! [`MigrationPlan`] is the canonical T3 output the runner T4 will
//! consume; segment kinds tell the runner how to dispatch each
//! group of statements.

pub mod attune;
pub mod build_match;
pub mod compose;
pub mod diff;
pub mod docs;
pub mod guard;
pub mod ledger;
pub mod naming;
pub mod policy;
pub mod projection;
pub mod repair;
pub mod reset;
pub mod runner;
pub mod schema;
pub mod seed;
pub mod segment;
pub mod snapshot;
pub mod sql;
pub mod status;
pub mod target;
pub mod verify;

pub use attune::{
    AttuneDiagnostic, AttuneEntry, AttuneEntryKind, AttuneError, AttuneMode, AttuneRefusal,
    AttuneReport, AttuneRequest, attune,
};
pub use build_match::{
    DriftDiagnostic, DriftKind, classify_bucket as build_classify_bucket,
    classify_bucket_with_pending as build_classify_bucket_with_pending, classify_filesystem_drift,
};
pub use compose::{
    AppLifecycle, ComposeError, ComposeReport, ComposeRequest, ComposedBucket,
    PENDING_FORMAT_VERSION, PendingLoadError, PendingPlan, compose, load_pending,
    parse_pending_bytes,
};
pub use diff::{
    Classification, ColumnChange, EnumVariantAnchor, EnumVariantAnchorKind, SchemaDelta,
    SchemaOperation, diff_bucket_maps,
};
pub use docs::{DocsError, DocsReport, generate_docs, render_inventory, render_model_page};
pub use guard::{
    DEFAULT_TIMEOUT as GUARD_DEFAULT_TIMEOUT, GuardError, LOCK_FILE_NAME, WorkspaceGuard,
    acquire as acquire_workspace_lock,
};
pub use ledger::{
    CHECKSUM_LEN, CHECKSUM_PREFIX, ChecksumFormatError, ChecksumFormatErrorKind, ChecksumMismatch,
    ChecksumSide, ExecutionMode, LEDGER_TABLE_DDL, LedgerRow, LedgerStatus, LedgerSummaryRow,
    SHA256_HEX_LEN, VerifyError, bootstrap as bootstrap_ledger, compute_checksum,
    insert_pending as insert_pending_ledger_row, mark_applied as mark_ledger_applied,
    mark_failed as mark_ledger_failed, mark_partial as mark_ledger_partial,
    select_all as select_all_ledger_rows, update_progress as update_ledger_progress,
    validate_checksum_format, verify_checksum,
};
pub use naming::{
    MAX_SLUG_LEN, down_filename, pending_json_filename, sanitize_slug, up_filename, version_id,
    version_prefix,
};
pub use policy::{OutOfOrderPolicy, is_localhost_connection};
pub use projection::{BucketKey, ProjectionError, project_from_inventory};
pub use repair::{
    LedgerChange, PartialApplyResolution, RepairConfirmation, RepairError, RepairReport,
    SnapshotChange, repair_checksum_drift, repair_partial_apply, repair_resume_partial_apply,
    repair_snapshot_rebuild,
};
pub use reset::{
    ReplayedMigration, ResetError, ResetRefusal, ResetReport, ResetRequest, reset_app_database,
};
pub use runner::{
    LossyRollbackPolicy, RollbackError, RollbackReport, RunReport, RunnerCtx, RunnerError,
    advisory_lock_key, apply_plan, baseline_plan, fake_apply_plan, rollback_plan,
};
pub use schema::{
    AppliedSchema, ColumnSchema, CustomPkKindSchema, EnumSchema, ForeignKeySchema, FtsSchema,
    IndexColumnSchema, IndexKindSchema, IndexNullsOrderSchema, IndexOrderSchema, IndexSchema,
    IndexTargetSchema, IndexTypeSchema, OnDeleteSchema, PartitionSchema, PkKindSchema,
    PrimaryKeySchema, RelationKindSchema, SNAPSHOT_FORMAT_VERSION, TableSchema,
};
pub use seed::{
    DiscoveredSeed, SEED_LEDGER_TABLE_DDL, SEEDS_DIRNAME, SeedError, SeedOutcome, SeedReport,
    SeedReportEntry, bootstrap as bootstrap_seed_ledger, compute_seed_checksum,
    derive_per_database_url, discover_seeds,
    fetch_recorded_checksum as fetch_recorded_seed_checksum,
    insert_recorded as insert_recorded_seed, run_seeds,
};
pub use segment::{MigrationPlan, Segment, SegmentKind, plan_delta};
pub use snapshot::{
    SnapshotError, load_snapshot, parse_snapshot_bytes, save_snapshot, serialize_snapshot,
};
pub use sql::{LossyRollbackKind, LossyRollbackWarning, OperationSql, SqlEmitError, lower_delta};
pub use status::{StatusReport, render as render_status};
pub use target::{
    FilesystemBucket, GLOBAL_BUCKET_DIRNAME, MIGRATIONS_DIR, MODELS_INVENTORY_FILENAME,
    PENDING_DIR, SNAPSHOT_FILENAME, app_dirname, app_label_from_dirname, bucket_dir, database_dir,
    migrations_root, pending_database_dir, pending_json_path, pending_root, scan_filesystem,
    snapshot_path,
};
pub use verify::{
    VerifyDiagnostic, VerifyReport, VerifyRunError, VerifySeverity, verify, verify_with_policy,
};
