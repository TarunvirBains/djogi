//! Project static-lifetime descriptors (collected via
//! `inventory::submit!`) into owned [`AppliedSchema`] data — one
//! [`AppliedSchema`] per `(database, app)` bucket per the snapshot
//! contract.
//!
//! The descriptor types in [`crate::descriptor`] are populated at
//! compile time and use `&'static` references throughout; the snapshot
//! types in [`crate::migrate::schema`] are owned so they can survive
//! load-from-disk. This module is the single boundary that does the
//! translation.
//!
//! # Per-bucket projection
//!
//! Phase 7's snapshot contract is one file per `(database, app)`
//! pair, with the synthetic global bucket
//! (`("main", "")`) always present (per Phase 7-Zero §4B). The
//! projection therefore returns a [`BTreeMap`] keyed by [`BucketKey`]
//! rather than a single [`AppliedSchema`]. The mapping rule:
//!
//! - Models with no `#[model(app = ...)]` declaration land in the
//!   synthetic global bucket — `("main", "")`.
//! - Models with `#[model(app = SomeApp)]` land in
//!   `(SomeApp::DATABASE, SomeApp::LABEL)`.
//! - Enums and FK targets are placed alongside the models that
//!   reference them — see "Cross-bucket FK targets" below for the
//!   resolution rule.
//!
//! # Determinism
//!
//! Each per-bucket [`AppliedSchema`] is sorted alphabetically:
//! `models` is a `BTreeMap` (alphabetical by table name), `enums` is
//! a `BTreeMap` (alphabetical by Postgres type name), `indexes` is
//! sorted by `(table, name)`, `registered_apps` is sorted
//! alphabetically. Struct field declarations are alphabetical so
//! serde emits keys alphabetically. `columns` preserves descriptor
//! declaration order — Postgres `CREATE TABLE` cares about column
//! order.
//!
//! # Cross-bucket FK targets
//!
//! `FieldDescriptor.target_type_name` carries the target's Rust type
//! name. The snapshot needs the Postgres table name. The projection
//! builds a global `type_name → table_name` map across **every**
//! bucket so that an FK from `billing.invoices.user_id` to
//! `users.users.id` resolves cleanly even though the two tables
//! live in different buckets. Cross-database FKs are rejected by
//! Phase 7's differ (T2), not here — the projection's job is purely
//! to record the FK target as a (table, column) pair.
//!
//! # Identity invariants
//!
//! Three uniqueness rules are enforced; violations return
//! [`ProjectionError`] without partial state:
//!
//! 1. Every [`crate::descriptor::ModelDescriptor::type_name`] is
//!    globally unique. Two models cannot share a Rust type name even
//!    across modules — otherwise FK target resolution would silently
//!    pick the wrong table.
//! 2. Every `(database, app, table_name)` is unique within a
//!    bucket. Two models cannot land at the same Postgres table
//!    inside the same `(database, app)` bucket.
//! 3. Every [`crate::descriptor::EnumDescriptor::postgres_type`] is
//!    globally unique. Two enums cannot share a `CREATE TYPE` name.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use crate::apps::{AppDescriptor, AppRegistry};
use crate::descriptor::{
    EnumDescriptor, FieldDescriptor, IndexKind, IndexNullsOrder, IndexOrder, IndexSpec,
    IndexTarget, IndexType, ModelDescriptor, PartitionSpec, PkType,
};
use crate::fts::FtsDescriptor;
use crate::relation::{OnDelete, RelationKind};

use super::schema::{
    AppliedSchema, ColumnSchema, CustomPkKindSchema, EnumSchema, ForeignKeySchema, FtsSchema,
    IndexColumnSchema, IndexKindSchema, IndexNullsOrderSchema, IndexOrderSchema, IndexSchema,
    IndexTargetSchema, IndexTypeSchema, OnDeleteSchema, PartitionSchema, PkKindSchema,
    PrimaryKeySchema, RelationKindSchema, SNAPSHOT_FORMAT_VERSION, TableSchema,
};

/// Identity of a snapshot bucket — `(database_target, app_label)`.
///
/// The synthetic global bucket is `BucketKey { database:
/// "main".into(), app: "".into() }` and is always present in any
/// projection result, even if no models live in it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BucketKey {
    /// Database target — `"main"`, `"crud_log"`, `"event_log"`, or a
    /// user-defined target. Each target gets its own migration
    /// ledger and advisory lock per Phase 7's contract.
    pub database: String,
    /// App label — the `#[djogi::apps!]` `LABEL` value. Empty
    /// string `""` for the synthetic global bucket (models without
    /// `#[model(app = ...)]`).
    pub app: String,
}

/// Errors surfaced by the descriptor projection.
///
/// Surfaces correctness failures that would otherwise produce a
/// silently-wrong snapshot: duplicate model identities, duplicate
/// table names within a bucket, duplicate enum SQL names, FK targets
/// pointing at unregistered models. Every variant carries enough
/// context for an actionable operator message.
#[derive(Debug)]
pub enum ProjectionError {
    /// Two `ModelDescriptor`s share the same Rust `type_name`. Names
    /// must be globally unique because FK target resolution keys off
    /// `type_name`.
    DuplicateModelTypeName {
        /// The repeated `type_name`.
        type_name: String,
        /// The Postgres table the first model registered.
        first_table: String,
        /// The Postgres table the second model registered.
        second_table: String,
    },
    /// Two models within the same `(database, app)` bucket land at
    /// the same Postgres table. The bucket can hold at most one row
    /// per table name.
    DuplicateTableInBucket {
        bucket: BucketKey,
        table: String,
        /// The Rust type name of the first model.
        first_type: String,
        /// The Rust type name of the second model.
        second_type: String,
    },
    /// Two `EnumDescriptor`s share the same Postgres type name.
    DuplicateEnumPostgresType {
        postgres_type: String,
        first_rust_type: String,
        second_rust_type: String,
    },
    /// `#[model(app = SomeApp)]` references an app that is not
    /// declared in `inventory::iter::<AppDescriptor>` and not the
    /// synthetic global bucket. Either the user forgot to register
    /// the app via `djogi::apps!`, or the app label is misspelled.
    UnknownAppLabel {
        /// The label the model declared.
        app_label: String,
        /// The model that referenced it.
        model_table: String,
    },
}

impl std::fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProjectionError::DuplicateModelTypeName {
                type_name,
                first_table,
                second_table,
            } => write!(
                f,
                "two `#[model]`s share the Rust type name `{type_name}`: \
                 tables `{first_table}` and `{second_table}`. Type names must \
                 be globally unique because FK target resolution keys off \
                 `type_name`. Rename one of the structs."
            ),
            ProjectionError::DuplicateTableInBucket {
                bucket,
                table,
                first_type,
                second_type,
            } => write!(
                f,
                "two models in bucket (database={}, app={}) land at the same \
                 Postgres table `{table}`: types `{first_type}` and `{second_type}`. \
                 Each `(database, app, table)` triple may carry at most one model.",
                bucket.database, bucket.app
            ),
            ProjectionError::DuplicateEnumPostgresType {
                postgres_type,
                first_rust_type,
                second_rust_type,
            } => write!(
                f,
                "two `#[derive(DjogiEnum)]` types share the Postgres type \
                 name `{postgres_type}`: `{first_rust_type}` and `{second_rust_type}`. \
                 Postgres `CREATE TYPE` names must be globally unique."
            ),
            ProjectionError::UnknownAppLabel {
                app_label,
                model_table,
            } => write!(
                f,
                "model `{model_table}` declares `#[model(app = ...)]` resolving \
                 to label `{app_label}`, but no app with that label is registered \
                 via `djogi::apps!`. Either declare the app or fix the label."
            ),
        }
    }
}

impl std::error::Error for ProjectionError {}

/// Project the global descriptor inventory into per-bucket
/// [`AppliedSchema`]s.
///
/// Walks `inventory::iter::<ModelDescriptor>`,
/// `inventory::iter::<EnumDescriptor>`, and [`AppRegistry::all`] —
/// the production entry point. Use [`project_from_iters`] when you
/// need to project from explicit iterables (tests).
pub fn project_from_inventory() -> Result<BTreeMap<BucketKey, AppliedSchema>, ProjectionError> {
    project_from_iters(
        inventory::iter::<ModelDescriptor>(),
        inventory::iter::<EnumDescriptor>(),
        AppRegistry::all().iter(),
        rfc3339_now_seconds(),
    )
}

/// Project from explicit descriptor iterables. Lower-level entry
/// point used by [`project_from_inventory`] and by tests that need
/// to feed in synthetic descriptors.
///
/// `generated_at` is taken as a parameter so tests can pin a
/// deterministic timestamp.
///
/// Returns a `BTreeMap` keyed by [`BucketKey`] — one
/// [`AppliedSchema`] per `(database, app)`. The synthetic global
/// bucket (`("main", "")`) is always present in the result, even if
/// no models live in it. Apps in the input that have zero models
/// also appear with empty `models` / `indexes` slots so the
/// snapshot directory layout stays consistent.
pub(crate) fn project_from_iters<'a, M, E, A>(
    models: M,
    enums: E,
    apps: A,
    generated_at: String,
) -> Result<BTreeMap<BucketKey, AppliedSchema>, ProjectionError>
where
    M: IntoIterator<Item = &'a ModelDescriptor>,
    E: IntoIterator<Item = &'a EnumDescriptor>,
    A: IntoIterator<Item = &'a AppDescriptor>,
{
    let models: Vec<&ModelDescriptor> = models.into_iter().collect();
    let apps: Vec<&AppDescriptor> = apps.into_iter().collect();

    // Build label → AppDescriptor map. Always includes the synthetic
    // global bucket per AppRegistry contract.
    let mut label_to_app: BTreeMap<&str, &AppDescriptor> = BTreeMap::new();
    label_to_app.insert(AppDescriptor::GLOBAL_LABEL, &AppDescriptor::GLOBAL);
    for a in &apps {
        label_to_app.insert(a.label, a);
    }

    // First pass — duplicate type_name detection across the entire
    // inventory (B-1). Reject before doing per-bucket work.
    let mut type_to_table: BTreeMap<&str, &str> = BTreeMap::new();
    for m in &models {
        if let Some(prev_table) = type_to_table.insert(m.type_name, m.table_name)
            && prev_table != m.table_name
        {
            return Err(ProjectionError::DuplicateModelTypeName {
                type_name: m.type_name.to_string(),
                first_table: prev_table.to_string(),
                second_table: m.table_name.to_string(),
            });
        } else if type_to_table.get(m.type_name) == Some(&m.table_name)
            && type_to_table
                .values()
                .filter(|v| **v == m.table_name)
                .count()
                == 1
        {
            // Already inserted; idempotent reinsert is fine.
        }
    }

    // Second pass — group models by bucket. Validate the model's
    // declared app exists.
    let mut bucket_models: BTreeMap<BucketKey, Vec<&ModelDescriptor>> = BTreeMap::new();
    for m in &models {
        let label = m.app.unwrap_or(AppDescriptor::GLOBAL_LABEL);
        let app = label_to_app
            .get(label)
            .ok_or_else(|| ProjectionError::UnknownAppLabel {
                app_label: label.to_string(),
                model_table: m.table_name.to_string(),
            })?;
        let bucket = BucketKey {
            database: app.database.to_string(),
            app: app.label.to_string(),
        };
        bucket_models.entry(bucket).or_default().push(m);
    }

    // Ensure every registered app has a bucket — even if it has no
    // models. Phase 7's filesystem layout (`migrations/<db>/<app>/`)
    // expects the directory; downstream consumers (D004 build.rs
    // diagnostic) compare snapshots against the filesystem listing.
    for app in label_to_app.values() {
        let bucket = BucketKey {
            database: app.database.to_string(),
            app: app.label.to_string(),
        };
        bucket_models.entry(bucket).or_default();
    }

    // Enums — global namespace, but emitted into every bucket whose
    // models reference them. For now, emit each enum into every
    // bucket that holds at least one model (simple and correct for
    // 0.1.0; T2's differ can refine if needed). Enforce duplicate
    // postgres_type detection.
    let mut enum_map: BTreeMap<&str, EnumSchema> = BTreeMap::new();
    let mut enum_rust_type_for_pg: BTreeMap<&str, &str> = BTreeMap::new();
    for e in enums {
        if let Some(prev_rust) = enum_rust_type_for_pg.insert(e.postgres_type, e.type_name) {
            return Err(ProjectionError::DuplicateEnumPostgresType {
                postgres_type: e.postgres_type.to_string(),
                first_rust_type: prev_rust.to_string(),
                second_rust_type: e.type_name.to_string(),
            });
        }
        enum_map.insert(
            e.postgres_type,
            EnumSchema {
                name: e.postgres_type.to_string(),
                variants: e.variants.iter().map(|v| v.to_string()).collect(),
            },
        );
    }

    // Sorted registered_apps — stable across runs, deduped, includes
    // the synthetic global bucket.
    let mut registered_apps: Vec<String> = label_to_app.keys().map(|k| k.to_string()).collect();
    registered_apps.sort();
    registered_apps.dedup();

    // Build each bucket's AppliedSchema.
    let mut out: BTreeMap<BucketKey, AppliedSchema> = BTreeMap::new();
    for (bucket, ms) in bucket_models {
        let mut tables: BTreeMap<String, TableSchema> = BTreeMap::new();
        let mut indexes: Vec<IndexSchema> = Vec::new();
        for m in &ms {
            let projected = project_model(m, &type_to_table);
            match tables.entry(projected.table.clone()) {
                Entry::Vacant(v) => {
                    for idx in m.indexes {
                        indexes.push(project_index(idx, m.table_name));
                    }
                    v.insert(projected);
                }
                Entry::Occupied(occ) => {
                    return Err(ProjectionError::DuplicateTableInBucket {
                        bucket: bucket.clone(),
                        table: projected.table,
                        first_type: occ.get().table.clone(),
                        second_type: m.type_name.to_string(),
                    });
                }
            }
        }
        indexes.sort_by(|a, b| {
            (a.table.as_str(), a.name.as_str()).cmp(&(b.table.as_str(), b.name.as_str()))
        });

        // Per-bucket enum projection — for now, every bucket sees the
        // global enum set. Phase 7.5 may scope enums per app.
        let bucket_enums: BTreeMap<String, EnumSchema> = enum_map
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();

        let schema = AppliedSchema {
            djogi_version: env!("CARGO_PKG_VERSION").to_string(),
            enums: bucket_enums,
            format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
            generated_at: generated_at.clone(),
            indexes,
            models: tables,
            registered_apps: registered_apps.clone(),
        };
        out.insert(bucket, schema);
    }

    Ok(out)
}

/// Returns the current UTC time as RFC 3339, second precision —
/// e.g. `2026-04-25T13:18:57Z`. Uses `time::OffsetDateTime::now_utc`.
///
/// Sub-second precision is stripped so the snapshot's `generated_at`
/// is byte-stable when the same descriptor inventory is projected
/// twice in close succession (e.g. `compose` followed by `verify`).
pub(crate) fn rfc3339_now_seconds() -> String {
    use time::OffsetDateTime;
    let now = OffsetDateTime::now_utc();
    let secs = now.unix_timestamp();
    let trimmed = OffsetDateTime::from_unix_timestamp(secs).unwrap_or(now);
    let format = time::format_description::well_known::Rfc3339;
    trimmed
        .format(&format)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn project_model(m: &ModelDescriptor, type_to_table: &BTreeMap<&str, &str>) -> TableSchema {
    let columns: Vec<ColumnSchema> = m
        .fields
        .iter()
        .map(|f| project_column(f, m, type_to_table))
        .collect();

    let primary_key = project_primary_key(&m.pk_type);

    TableSchema {
        app: m.app.map(|s| s.to_string()),
        columns,
        fts: m.fts.as_ref().map(project_fts),
        is_through: m.is_through,
        moved_from_app: m.moved_from_app.map(|s| s.to_string()),
        partition: m.partition_by.as_ref().map(project_partition),
        primary_key,
        rationale: m.rationale.map(|s| s.to_string()),
        renamed_from: m.renamed_from.map(|s| s.to_string()),
        rls_enabled: m.tenant_key.is_some(),
        table: m.table_name.to_string(),
        tenant_key: m.tenant_key.map(|s| s.to_string()),
    }
}

fn project_column(
    f: &FieldDescriptor,
    parent: &ModelDescriptor,
    type_to_table: &BTreeMap<&str, &str>,
) -> ColumnSchema {
    let foreign_key = if f.relation_kind.is_some() {
        f.target_type_name.map(|target| {
            let ref_table = type_to_table
                .get(target)
                .copied()
                .unwrap_or(target)
                .to_string();
            ForeignKeySchema {
                ref_column: "id".to_string(),
                ref_table,
            }
        })
    } else {
        None
    };

    let default_sql = if f.name == "id" {
        pk_default_sql(&parent.pk_type)
    } else {
        None
    };

    ColumnSchema {
        check: None,
        default_sql,
        foreign_key,
        index_type: f.index_type.map(project_index_type),
        indexed: f.indexed,
        max_length: f.max_length,
        name: f.name.to_string(),
        nullable: f.nullable,
        on_delete: if f.relation_kind.is_some() {
            Some(project_on_delete(f.on_delete.unwrap_or(OnDelete::Restrict)))
        } else {
            None
        },
        outbox_exclude: f.outbox_exclude,
        rationale: f.rationale.map(|s| s.to_string()),
        relation_kind: f.relation_kind.map(project_relation_kind),
        renamed_from: f.renamed_from.map(|s| s.to_string()),
        sequence_within: f.sequence_within.map(|s| s.to_string()),
        sql_type: f.sql_type.to_string(),
        unique: f.unique,
    }
}

fn project_primary_key(pk: &PkType) -> PrimaryKeySchema {
    match pk {
        PkType::HeerId => PrimaryKeySchema {
            columns: vec!["id".to_string()],
            kind: PkKindSchema::HeerId,
        },
        PkType::HeerIdDesc => PrimaryKeySchema {
            columns: vec!["id".to_string()],
            kind: PkKindSchema::HeerIdRecencyBiased,
        },
        PkType::RanjId => PrimaryKeySchema {
            columns: vec!["id".to_string()],
            kind: PkKindSchema::RanjId,
        },
        PkType::RanjIdDesc => PrimaryKeySchema {
            columns: vec!["id".to_string()],
            kind: PkKindSchema::RanjIdRecencyBiased,
        },
        PkType::Serial => PrimaryKeySchema {
            columns: vec!["id".to_string()],
            kind: PkKindSchema::Serial,
        },
        PkType::None => PrimaryKeySchema {
            columns: Vec::new(),
            kind: PkKindSchema::None,
        },
        PkType::Composite(cols) => PrimaryKeySchema {
            columns: cols.iter().map(|c| c.to_string()).collect(),
            kind: PkKindSchema::Composite,
        },
        PkType::Custom(c) => PrimaryKeySchema {
            columns: vec!["id".to_string()],
            kind: PkKindSchema::Custom(CustomPkKindSchema {
                default_sql: c.default_sql.to_string(),
                sql_type: c.sql_type.to_string(),
                type_name: c.type_name.to_string(),
            }),
        },
    }
}

fn pk_default_sql(pk: &PkType) -> Option<String> {
    match pk {
        PkType::HeerId => Some("generate_id()".to_string()),
        PkType::HeerIdDesc => Some("generate_id_desc()".to_string()),
        PkType::RanjId => Some("generate_ranj_id()".to_string()),
        PkType::RanjIdDesc => Some("generate_ranj_id_desc()".to_string()),
        PkType::Serial => None,
        PkType::None => None,
        PkType::Composite(_) => None,
        PkType::Custom(c) if c.default_sql.is_empty() => None,
        PkType::Custom(c) => Some(c.default_sql.to_string()),
    }
}

fn project_fts(f: &FtsDescriptor) -> FtsSchema {
    FtsSchema {
        column: f.column.to_string(),
        dictionary: f.dictionary.to_string(),
        source: f.source.to_string(),
    }
}

fn project_partition(p: &PartitionSpec) -> PartitionSchema {
    match p {
        PartitionSpec::Range { column } => PartitionSchema::Range {
            column: column.to_string(),
        },
        PartitionSpec::Hash { column, partitions } => PartitionSchema::Hash {
            column: column.to_string(),
            partitions: *partitions,
        },
    }
}

fn project_index(idx: &IndexSpec, table: &str) -> IndexSchema {
    IndexSchema {
        extension_dependency: idx.extension_dependency.map(|s| s.to_string()),
        include: idx.include.iter().map(|s| s.to_string()).collect(),
        index_type: project_index_type(idx.index_type),
        kind: project_index_kind(idx.kind),
        name: idx.name.to_string(),
        nulls_not_distinct: idx.nulls_not_distinct,
        predicate: idx.predicate.map(|s| s.to_string()),
        requires_out_of_transaction: idx.requires_out_of_transaction,
        table: table.to_string(),
        target: project_index_target(&idx.target),
    }
}

fn project_index_type(t: IndexType) -> IndexTypeSchema {
    match t {
        IndexType::BTree => IndexTypeSchema::BTree,
        IndexType::Gin => IndexTypeSchema::Gin,
        IndexType::Gist => IndexTypeSchema::Gist,
        IndexType::Hash => IndexTypeSchema::Hash,
        IndexType::Spgist => IndexTypeSchema::Spgist,
        IndexType::Brin => IndexTypeSchema::Brin,
    }
}

fn project_index_kind(k: IndexKind) -> IndexKindSchema {
    match k {
        IndexKind::NonUnique => IndexKindSchema::NonUnique,
        IndexKind::UniqueConstraint => IndexKindSchema::UniqueConstraint,
        IndexKind::UniqueIndex => IndexKindSchema::UniqueIndex,
    }
}

fn project_index_target(t: &IndexTarget) -> IndexTargetSchema {
    match t {
        IndexTarget::Columns(cols) => IndexTargetSchema::Columns(
            cols.iter()
                .map(|c| IndexColumnSchema {
                    name: c.name.to_string(),
                    nulls: project_index_nulls(c.nulls),
                    opclass: c.opclass.map(|s| s.to_string()),
                    order: project_index_order(c.order),
                })
                .collect(),
        ),
        IndexTarget::Expression(expr) => IndexTargetSchema::Expression(expr.to_string()),
    }
}

fn project_index_order(o: IndexOrder) -> IndexOrderSchema {
    match o {
        IndexOrder::Asc => IndexOrderSchema::Asc,
        IndexOrder::Desc => IndexOrderSchema::Desc,
    }
}

fn project_index_nulls(n: IndexNullsOrder) -> IndexNullsOrderSchema {
    match n {
        IndexNullsOrder::Default => IndexNullsOrderSchema::Default,
        IndexNullsOrder::First => IndexNullsOrderSchema::First,
        IndexNullsOrder::Last => IndexNullsOrderSchema::Last,
    }
}

fn project_on_delete(o: OnDelete) -> OnDeleteSchema {
    // OnDelete is `#[non_exhaustive]` for cross-crate consumers; we
    // are inside the same crate so the compiler does not enforce a
    // wildcard. Match exhaustively — adding a future variant flags
    // this site for explicit mapping.
    match o {
        OnDelete::Cascade => OnDeleteSchema::Cascade,
        OnDelete::Restrict => OnDeleteSchema::Restrict,
        OnDelete::SetNull => OnDeleteSchema::SetNull,
        OnDelete::SetDefault => OnDeleteSchema::SetDefault,
        OnDelete::Protect => OnDeleteSchema::Restrict,
        OnDelete::DoNothing => OnDeleteSchema::NoAction,
    }
}

fn project_relation_kind(k: RelationKind) -> RelationKindSchema {
    match k {
        RelationKind::ForeignKey => RelationKindSchema::ForeignKey,
        RelationKind::OneToOne => RelationKindSchema::OneToOne,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::AppDescriptor;
    use crate::descriptor::{
        EnumDescriptor, FieldDescriptor, FieldSqlType, IndexColumnSpec, IndexKind, IndexSpec,
        IndexTarget, IndexType, ModelDescriptor, PkType,
    };

    fn synth_model(table: &'static str, type_name: &'static str) -> ModelDescriptor {
        ModelDescriptor {
            type_name,
            table_name: table,
            pk_type: PkType::HeerIdDesc,
            fields: &[],
            partition_by: None,
            has_outbox: false,
            idempotency_key: None,
            tenant_key: None,
            cache_ttl: None,
            rationale: None,
            indexes: &[],
            is_through: false,
            fts: None,
            app: None,
            moved_from_app: None,
            renamed_from: None,
        }
    }

    fn synth_app(label: &'static str, database: &'static str) -> AppDescriptor {
        AppDescriptor {
            label,
            database,
            renamed_from: None,
            tombstone: false,
        }
    }

    fn empty_global() -> BucketKey {
        BucketKey {
            database: "main".to_string(),
            app: "".to_string(),
        }
    }

    #[test]
    fn empty_inventory_yields_only_synthetic_global_bucket() {
        let buckets = project_from_iters(
            std::iter::empty::<&ModelDescriptor>(),
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        assert_eq!(buckets.len(), 1);
        let global = buckets.get(&empty_global()).expect("global bucket present");
        assert!(global.models.is_empty());
        assert!(global.indexes.is_empty());
        assert_eq!(global.registered_apps, vec!["".to_string()]);
    }

    #[test]
    fn models_without_app_land_in_global_bucket() {
        let m = synth_model("widgets", "Widget");
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let global = buckets.get(&empty_global()).expect("global");
        assert_eq!(global.models.len(), 1);
        assert!(global.models.contains_key("widgets"));
    }

    #[test]
    fn models_with_app_land_in_app_bucket() {
        let billing = synth_app("billing", "main");
        let m = ModelDescriptor {
            app: Some("billing"),
            ..synth_model("invoices", "Invoice")
        };
        let buckets = project_from_iters([&m], [], [&billing], "2026-04-25T00:00:00Z".to_string())
            .expect("ok");
        let billing_bucket = BucketKey {
            database: "main".to_string(),
            app: "billing".to_string(),
        };
        let bb = buckets
            .get(&billing_bucket)
            .expect("billing bucket present");
        assert_eq!(bb.models.len(), 1);
        let global = buckets.get(&empty_global()).expect("global still present");
        assert!(global.models.is_empty());
    }

    #[test]
    fn separate_databases_yield_separate_buckets() {
        let crud = synth_app("crud_log_app", "crud_log");
        let m_main = synth_model("widgets", "Widget");
        let m_crud = ModelDescriptor {
            app: Some("crud_log_app"),
            ..synth_model("audit_rows", "AuditRow")
        };
        let buckets = project_from_iters(
            [&m_main, &m_crud],
            [],
            [&crud],
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        assert_eq!(
            buckets
                .keys()
                .map(|b| (b.database.as_str(), b.app.as_str()))
                .collect::<Vec<_>>(),
            vec![("crud_log", "crud_log_app"), ("main", "")]
        );
    }

    #[test]
    fn duplicate_type_name_errors() {
        let a = synth_model("widgets_a", "Widget");
        let b = synth_model("widgets_b", "Widget");
        let err = project_from_iters(
            [&a, &b],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect_err("must reject");
        match err {
            ProjectionError::DuplicateModelTypeName { type_name, .. } => {
                assert_eq!(type_name, "Widget");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn duplicate_table_in_bucket_errors() {
        let a = synth_model("widgets", "WidgetA");
        let b = synth_model("widgets", "WidgetB");
        let err = project_from_iters(
            [&a, &b],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect_err("must reject");
        match err {
            ProjectionError::DuplicateTableInBucket { table, .. } => {
                assert_eq!(table, "widgets");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn same_table_name_in_different_buckets_is_fine() {
        let billing = synth_app("billing", "main");
        let users = synth_app("users", "main");
        let m_billing = ModelDescriptor {
            app: Some("billing"),
            ..synth_model("settings", "BillingSettings")
        };
        let m_users = ModelDescriptor {
            app: Some("users"),
            ..synth_model("settings", "UserSettings")
        };
        let buckets = project_from_iters(
            [&m_billing, &m_users],
            [],
            [&billing, &users],
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok — distinct buckets so `settings` is unique within each");
        let bk_billing = BucketKey {
            database: "main".to_string(),
            app: "billing".to_string(),
        };
        let bk_users = BucketKey {
            database: "main".to_string(),
            app: "users".to_string(),
        };
        assert!(buckets[&bk_billing].models.contains_key("settings"));
        assert!(buckets[&bk_users].models.contains_key("settings"));
    }

    #[test]
    fn duplicate_enum_postgres_type_errors() {
        let e1 = EnumDescriptor {
            type_name: "VehicleStatus",
            postgres_type: "vehicle_status",
            variants: &["active"],
        };
        let e2 = EnumDescriptor {
            type_name: "OtherEnum",
            postgres_type: "vehicle_status",
            variants: &["pending"],
        };
        let err = project_from_iters(
            std::iter::empty::<&ModelDescriptor>(),
            [&e1, &e2],
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect_err("must reject");
        match err {
            ProjectionError::DuplicateEnumPostgresType { postgres_type, .. } => {
                assert_eq!(postgres_type, "vehicle_status");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn unknown_app_label_errors() {
        // Model declares app = "nonexistent" but no AppDescriptor with
        // that label is in the inventory. Reject.
        let m = ModelDescriptor {
            app: Some("nonexistent"),
            ..synth_model("widgets", "Widget")
        };
        let err = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect_err("must reject");
        match err {
            ProjectionError::UnknownAppLabel { app_label, .. } => {
                assert_eq!(app_label, "nonexistent");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn fk_target_resolves_to_table_name() {
        let owner = synth_model("owners", "Owner");
        let vehicle = ModelDescriptor {
            fields: &[FieldDescriptor {
                name: "owner_id",
                sql_type: FieldSqlType::BigInt,
                nullable: false,
                unique: false,
                indexed: true,
                max_length: None,
                renamed_from: None,
                rationale: None,
                outbox_exclude: false,
                sequence_within: None,
                index_type: None,
                relation_kind: Some(RelationKind::ForeignKey),
                on_delete: Some(OnDelete::Restrict),
                target_type_name: Some("Owner"),
                visage_map: &[],
            }],
            ..synth_model("vehicles", "Vehicle")
        };
        let buckets = project_from_iters(
            [&owner, &vehicle],
            [],
            [],
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let global = &buckets[&empty_global()];
        let owner_id = &global.models["vehicles"].columns[0];
        let fk = owner_id.foreign_key.as_ref().expect("fk present");
        assert_eq!(fk.ref_table, "owners");
    }

    #[test]
    fn cross_bucket_fk_resolves_via_global_type_lookup() {
        let billing = synth_app("billing", "main");
        let users = synth_app("users", "main");
        let user = ModelDescriptor {
            app: Some("users"),
            ..synth_model("users", "User")
        };
        let invoice = ModelDescriptor {
            app: Some("billing"),
            fields: &[FieldDescriptor {
                name: "user_id",
                sql_type: FieldSqlType::BigInt,
                nullable: false,
                unique: false,
                indexed: true,
                max_length: None,
                renamed_from: None,
                rationale: None,
                outbox_exclude: false,
                sequence_within: None,
                index_type: None,
                relation_kind: Some(RelationKind::ForeignKey),
                on_delete: Some(OnDelete::Restrict),
                target_type_name: Some("User"),
                visage_map: &[],
            }],
            ..synth_model("invoices", "Invoice")
        };
        let buckets = project_from_iters(
            [&user, &invoice],
            [],
            [&billing, &users],
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let bk_billing = BucketKey {
            database: "main".to_string(),
            app: "billing".to_string(),
        };
        let invoice_user_id = &buckets[&bk_billing].models["invoices"].columns[0];
        let fk = invoice_user_id.foreign_key.as_ref().expect("fk");
        // The FK target table resolves correctly even though the
        // target lives in a different bucket — global type_name map.
        assert_eq!(fk.ref_table, "users");
    }

    #[test]
    fn pk_default_sql_is_generate_id_desc_for_heer_id_desc() {
        let m = ModelDescriptor {
            fields: &[FieldDescriptor {
                name: "id",
                sql_type: FieldSqlType::BigInt,
                nullable: false,
                unique: false,
                indexed: false,
                max_length: None,
                renamed_from: None,
                rationale: None,
                outbox_exclude: false,
                sequence_within: None,
                index_type: None,
                relation_kind: None,
                on_delete: None,
                target_type_name: None,
                visage_map: &[],
            }],
            ..synth_model("widgets", "Widget")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let id_col = &buckets[&empty_global()].models["widgets"].columns[0];
        assert_eq!(id_col.default_sql.as_deref(), Some("generate_id_desc()"));
    }

    #[test]
    fn indexes_sorted_by_table_then_name() {
        static NAME_COLS: &[IndexColumnSpec] = &[IndexColumnSpec::simple("name")];
        static COLOR_COLS: &[IndexColumnSpec] = &[IndexColumnSpec::simple("color")];
        static IDX_PAIR: &[IndexSpec] = &[
            IndexSpec {
                name: "z_widget_idx",
                target: IndexTarget::Columns(NAME_COLS),
                kind: IndexKind::NonUnique,
                index_type: IndexType::BTree,
                predicate: None,
                include: &[],
                nulls_not_distinct: false,
                requires_out_of_transaction: false,
                extension_dependency: None,
            },
            IndexSpec {
                name: "a_widget_idx",
                target: IndexTarget::Columns(COLOR_COLS),
                kind: IndexKind::NonUnique,
                index_type: IndexType::BTree,
                predicate: None,
                include: &[],
                nulls_not_distinct: false,
                requires_out_of_transaction: false,
                extension_dependency: None,
            },
        ];
        let m = ModelDescriptor {
            indexes: IDX_PAIR,
            ..synth_model("widgets", "Widget")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let global = &buckets[&empty_global()];
        assert_eq!(global.indexes[0].name, "a_widget_idx");
        assert_eq!(global.indexes[1].name, "z_widget_idx");
    }

    #[test]
    fn registered_apps_includes_synthetic_global_and_user_apps() {
        let billing = synth_app("billing", "main");
        let users = synth_app("users", "main");
        let buckets = project_from_iters(
            std::iter::empty::<&ModelDescriptor>(),
            std::iter::empty::<&EnumDescriptor>(),
            [&users, &billing],
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let global = &buckets[&empty_global()];
        assert_eq!(
            global.registered_apps,
            vec!["".to_string(), "billing".to_string(), "users".to_string()]
        );
    }

    #[test]
    fn rfc3339_now_has_z_suffix_and_no_subseconds() {
        let s = rfc3339_now_seconds();
        assert!(
            s.ends_with('Z') || s.ends_with("+00:00"),
            "must be UTC: {s}"
        );
        assert!(!s.contains('.'), "must not have sub-second precision: {s}");
    }
}
