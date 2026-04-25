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
//!
//! Subsequent Phase 7 tasks add `diff`, `rename`, `sql`, `segment`,
//! `ledger`, `runner`, `guard`, `naming`, `target`, `docs` per the
//! v3 plan §5 file structure.
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

pub mod diff;
pub mod projection;
pub mod schema;
pub mod snapshot;

pub use diff::{Classification, ColumnChange, SchemaDelta, SchemaOperation, diff_bucket_maps};
pub use projection::{BucketKey, ProjectionError, project_from_inventory};
pub use schema::{
    AppliedSchema, ColumnSchema, CustomPkKindSchema, EnumSchema, ForeignKeySchema, FtsSchema,
    IndexColumnSchema, IndexKindSchema, IndexNullsOrderSchema, IndexOrderSchema, IndexSchema,
    IndexTargetSchema, IndexTypeSchema, OnDeleteSchema, PartitionSchema, PkKindSchema,
    PrimaryKeySchema, RelationKindSchema, SNAPSHOT_FORMAT_VERSION, TableSchema,
};
pub use snapshot::{
    SnapshotError, load_snapshot, parse_snapshot_bytes, save_snapshot, serialize_snapshot,
};
