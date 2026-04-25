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
//!
//! Subsequent Phase 7 tasks add `rename`, `ledger`, `runner`,
//! `guard`, `naming`, `target`, `docs` per the v3 plan §5 file
//! structure.
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

pub mod diff;
pub mod projection;
pub mod schema;
pub mod segment;
pub mod snapshot;
pub mod sql;

pub use diff::{
    Classification, ColumnChange, EnumVariantAnchor, EnumVariantAnchorKind, SchemaDelta,
    SchemaOperation, diff_bucket_maps,
};
pub use projection::{BucketKey, ProjectionError, project_from_inventory};
pub use schema::{
    AppliedSchema, ColumnSchema, CustomPkKindSchema, EnumSchema, ForeignKeySchema, FtsSchema,
    IndexColumnSchema, IndexKindSchema, IndexNullsOrderSchema, IndexOrderSchema, IndexSchema,
    IndexTargetSchema, IndexTypeSchema, OnDeleteSchema, PartitionSchema, PkKindSchema,
    PrimaryKeySchema, RelationKindSchema, SNAPSHOT_FORMAT_VERSION, TableSchema,
};
pub use segment::{MigrationPlan, Segment, SegmentKind, plan_delta};
pub use snapshot::{
    SnapshotError, load_snapshot, parse_snapshot_bytes, save_snapshot, serialize_snapshot,
};
pub use sql::{LossyRollbackKind, LossyRollbackWarning, OperationSql, SqlEmitError, lower_delta};
