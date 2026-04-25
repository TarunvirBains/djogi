//! Project static-lifetime descriptors (collected via
//! `inventory::submit!`) into owned [`AppliedSchema`] data.
//!
//! The descriptor types in [`crate::descriptor`] are populated at
//! compile time and use `&'static` references throughout; the snapshot
//! types in [`crate::migrate::schema`] are owned so they can survive
//! load-from-disk. This module is the single boundary that does the
//! translation.
//!
//! # Determinism
//!
//! All collections in the projection output are sorted by stable
//! identity at projection time:
//!
//! - `models` is a `BTreeMap`, automatically alphabetical by table name.
//! - `enums` is a `BTreeMap`, alphabetical by enum SQL name.
//! - `indexes` is sorted by `(table, name)` to keep the diff stable
//!   across model declaration order changes.
//! - `registered_apps` is sorted alphabetically.
//! - `columns` preserves descriptor declaration order — Postgres
//!   `CREATE TABLE` cares about column order, so the snapshot must
//!   too.
//!
//! # Cross-FK target lookup
//!
//! `FieldDescriptor.target_type_name` carries the target's Rust type
//! name (e.g. `"Owner"`). The snapshot needs the target's Postgres
//! table name (e.g. `"owners"`). The projection builds a
//! `type_name → table_name` map from the model iterable in a first
//! pass, then resolves FK targets in a second pass.
//!
//! Unresolvable target type names (FK to a model not registered in
//! the supplied iterable) currently keep the type name verbatim as
//! the `ref_table` value. T2's differ surfaces this as a clear
//! "unresolved foreign key target" diagnostic.

use std::collections::BTreeMap;

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

/// Project the global descriptor inventory into an [`AppliedSchema`].
///
/// Walks `inventory::iter::<ModelDescriptor>` for tables / columns /
/// indexes, `inventory::iter::<EnumDescriptor>` for Postgres
/// `CREATE TYPE` enums, and [`AppRegistry::all_labels`] for the
/// `registered_apps` field. The `generated_at` timestamp is set to
/// the current UTC time, RFC 3339, second precision.
///
/// Use [`project_from_iters`] when you need to project from explicit
/// iterables (tests, in-memory schemas).
pub fn project_from_inventory() -> AppliedSchema {
    project_from_iters(
        inventory::iter::<ModelDescriptor>(),
        inventory::iter::<EnumDescriptor>(),
        AppRegistry::all().iter(),
        rfc3339_now_seconds(),
    )
}

/// Project from explicit descriptor iterables. Lower-level entry point
/// used by [`project_from_inventory`] and by tests that need to feed
/// in synthetic descriptors.
///
/// `generated_at` is taken as a parameter (rather than fetched from
/// the system clock) so tests can pin a deterministic timestamp.
pub fn project_from_iters<'a, M, E, A>(
    models: M,
    enums: E,
    apps: A,
    generated_at: String,
) -> AppliedSchema
where
    M: IntoIterator<Item = &'a ModelDescriptor>,
    E: IntoIterator<Item = &'a EnumDescriptor>,
    A: IntoIterator<Item = &'a AppDescriptor>,
{
    let models: Vec<&ModelDescriptor> = models.into_iter().collect();

    // First pass: type-name → table-name map for FK target resolution.
    let mut type_to_table: BTreeMap<&str, &str> = BTreeMap::new();
    for m in &models {
        type_to_table.insert(m.type_name, m.table_name);
    }

    // Project each model into a TableSchema, harvesting indexes
    // separately because they live at the top level of the snapshot.
    let mut table_map: BTreeMap<String, TableSchema> = BTreeMap::new();
    let mut indexes: Vec<IndexSchema> = Vec::new();
    for m in &models {
        let table = project_model(m, &type_to_table);
        for idx in m.indexes {
            indexes.push(project_index(idx, m.table_name));
        }
        // The implicit primary-key index is captured in `TableSchema.primary_key`,
        // not in `indexes` — same convention Postgres uses.
        table_map.insert(table.table.clone(), table);
    }

    // Stable index ordering — sort by (table, name) so the diff is
    // not perturbed by model declaration order.
    indexes.sort_by(|a, b| {
        (a.table.as_str(), a.name.as_str()).cmp(&(b.table.as_str(), b.name.as_str()))
    });

    // Enums sorted alphabetically via BTreeMap. The map key is the
    // Postgres type name (the `CREATE TYPE` identifier) — NOT the
    // Rust type name. Two Rust types are not allowed to map to the
    // same Postgres type, so the key is unique.
    let mut enum_map: BTreeMap<String, EnumSchema> = BTreeMap::new();
    for e in enums.into_iter() {
        enum_map.insert(
            e.postgres_type.to_string(),
            EnumSchema {
                name: e.postgres_type.to_string(),
                variants: e.variants.iter().map(|v| v.to_string()).collect(),
            },
        );
    }

    // Apps — sorted alphabetically. The synthetic global bucket "" is
    // always present in AppRegistry::all() per the 7-Zero §4B invariant.
    let mut registered_apps: Vec<String> = apps.into_iter().map(|a| a.label.to_string()).collect();
    registered_apps.sort();
    registered_apps.dedup();

    AppliedSchema {
        djogi_version: env!("CARGO_PKG_VERSION").to_string(),
        enums: enum_map,
        format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
        generated_at,
        indexes,
        models: table_map,
        registered_apps,
    }
}

/// Returns the current UTC time as RFC 3339, second precision —
/// `2026-04-25T13:18:57Z`. Uses `time::OffsetDateTime::now_utc`.
pub fn rfc3339_now_seconds() -> String {
    use time::OffsetDateTime;
    let now = OffsetDateTime::now_utc();
    // Truncate to seconds — the snapshot timestamp is informational,
    // and stripping sub-second precision keeps generated_at byte-stable
    // when the same descriptor inventory is projected twice in close
    // succession (e.g. by `compose` followed by `verify`).
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
        // ModelDescriptor does not yet carry renamed_from — T2 of
        // Phase 7 adds the macro grammar + descriptor field. The
        // snapshot shape is forward-compatible: today this is always
        // None; T2's projection update flips the read site.
        renamed_from: None,
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

    // Compute the column's effective DEFAULT. PK columns inherit the
    // PK kind's server-side default; framework columns
    // (created_at / updated_at) do not — those defaults are the
    // descriptor field's responsibility once Phase 7 T2 widens the
    // shape to carry per-field defaults. For T1 we surface the PK
    // default and leave non-PK columns at None.
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
        PkType::Serial => None, // IDENTITY uses GENERATED BY DEFAULT, not DEFAULT
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
    // OnDelete is `#[non_exhaustive]` for cross-crate consumers, but we
    // are inside the same crate so the compiler does not enforce a
    // wildcard. Match exhaustively — adding a future variant flags this
    // site for explicit mapping rather than silently routing to a
    // potentially-wrong default.
    match o {
        OnDelete::Cascade => OnDeleteSchema::Cascade,
        OnDelete::Restrict => OnDeleteSchema::Restrict,
        OnDelete::SetNull => OnDeleteSchema::SetNull,
        OnDelete::SetDefault => OnDeleteSchema::SetDefault,
        // Protect aliases to RESTRICT at the SQL level (see
        // relation::on_delete docs).
        OnDelete::Protect => OnDeleteSchema::Restrict,
        // DoNothing maps to NO ACTION at the SQL level, distinct from
        // RESTRICT for DEFERRABLE constraints. Djogi emits IMMEDIATE
        // by default, so behaviour is RESTRICT-equivalent in practice,
        // but the snapshot preserves the distinction so the differ can
        // emit `ON DELETE NO ACTION` faithfully.
        OnDelete::DoNothing => OnDeleteSchema::NoAction,
    }
}

fn project_relation_kind(k: RelationKind) -> RelationKindSchema {
    // Same exhaustiveness reasoning as `project_on_delete`.
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
        }
    }

    #[test]
    fn empty_inventory_projects_empty_schema() {
        let schema = project_from_iters(
            std::iter::empty::<&ModelDescriptor>(),
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        );
        assert_eq!(schema.format_version, "1");
        assert_eq!(schema.generated_at, "2026-04-25T00:00:00Z");
        assert!(schema.models.is_empty());
        assert!(schema.indexes.is_empty());
        assert!(schema.enums.is_empty());
        assert!(schema.registered_apps.is_empty());
    }

    #[test]
    fn pk_kind_heer_id_desc_is_recency_biased_in_snapshot() {
        let m = synth_model("widgets", "Widget");
        let schema = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        );
        let table = schema.models.get("widgets").expect("widget table");
        assert_eq!(table.primary_key.kind, PkKindSchema::HeerIdRecencyBiased);
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
        let schema = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        );
        let id_col = &schema.models["widgets"].columns[0];
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
        let schema = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        );
        assert_eq!(schema.indexes[0].name, "a_widget_idx");
        assert_eq!(schema.indexes[1].name, "z_widget_idx");
    }

    fn synth_app(label: &'static str) -> AppDescriptor {
        AppDescriptor {
            label,
            database: "main",
            renamed_from: None,
            tombstone: false,
        }
    }

    #[test]
    fn registered_apps_deduped_and_sorted() {
        let app_billing = synth_app("billing");
        let app_users = synth_app("users");
        let app_dup = synth_app("billing");
        let schema = project_from_iters(
            std::iter::empty::<&ModelDescriptor>(),
            std::iter::empty::<&EnumDescriptor>(),
            [&app_users, &app_billing, &app_dup],
            "2026-04-25T00:00:00Z".to_string(),
        );
        assert_eq!(
            schema.registered_apps,
            vec!["billing".to_string(), "users".to_string()]
        );
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
        let schema = project_from_iters(
            [&owner, &vehicle],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        );
        let owner_id = &schema.models["vehicles"].columns[0];
        let fk = owner_id.foreign_key.as_ref().expect("fk present");
        assert_eq!(fk.ref_table, "owners");
        assert_eq!(fk.ref_column, "id");
    }

    #[test]
    fn fk_target_falls_back_to_type_name_when_unresolved() {
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
                on_delete: Some(OnDelete::Cascade),
                target_type_name: Some("UnregisteredType"),
                visage_map: &[],
            }],
            ..synth_model("vehicles", "Vehicle")
        };
        let schema = project_from_iters(
            [&vehicle],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        );
        let fk = schema.models["vehicles"].columns[0]
            .foreign_key
            .as_ref()
            .expect("fk present");
        // Unresolved target keeps the type name verbatim — T2's differ
        // surfaces this as a clear "unresolved foreign key target"
        // diagnostic rather than crashing here.
        assert_eq!(fk.ref_table, "UnregisteredType");
    }

    #[test]
    fn rls_enabled_follows_tenant_key() {
        let m = ModelDescriptor {
            tenant_key: Some("org_id"),
            ..synth_model("posts", "Post")
        };
        let schema = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        );
        let table = &schema.models["posts"];
        assert!(table.rls_enabled);
        assert_eq!(table.tenant_key.as_deref(), Some("org_id"));
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
