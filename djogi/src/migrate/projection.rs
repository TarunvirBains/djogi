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
    DeferrabilitySpec, EnumDescriptor, ExclusionConstraintSpec, ExclusionElement, FieldDescriptor,
    GeneratedColumnSpec, IndexKind, IndexNullsOrder, IndexOrder, IndexSpec, IndexTarget, IndexType,
    ModelDescriptor, PartitionSpec, PkType, RustSourceType,
};
use crate::fts::FtsDescriptor;
use crate::relation::{OnDelete, RelationKind};

use super::schema::{
    AppliedSchema, ColumnSchema, CustomPkKindSchema, EnumSchema, ExclusionConstraintSchema,
    ExclusionElementSchema, ForeignKeySchema, FtsSchema, GeneratedColumnSchema, IndexColumnSchema,
    IndexKindSchema, IndexNullsOrderSchema, IndexOrderSchema, IndexSchema, IndexTargetSchema,
    IndexTypeSchema, OnDeleteSchema, PartitionSchema, PkKindSchema, PrimaryKeySchema,
    RelationKindSchema, SNAPSHOT_FORMAT_VERSION, TableSchema,
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
    /// A `ForeignKey<T>` / `OneToOneField<T>` references a model in
    /// a different database target. Postgres FK constraints cannot
    /// span databases; cross-database FK references must use
    /// application-level joins or the outbox pattern instead.
    CrossDatabaseForeignKey {
        /// The bucket containing the FK source column.
        source_bucket: BucketKey,
        /// The source model's table.
        source_table: String,
        /// The source FK column.
        source_column: String,
        /// The bucket containing the FK target.
        target_bucket: BucketKey,
        /// The target model's table.
        target_table: String,
    },
    /// Codex round-7 WARN 6: two [`DeferrabilitySpec`] entries share
    /// the same `(model_type_name, field_name)` key but disagree on
    /// the deferrability values. Without this gate, `BTreeMap::collect`
    /// silently picks last-writer-wins from inventory iteration order,
    /// which is not deterministic across builds. Duplicate keys with
    /// matching values are accepted (idempotent re-emission); only
    /// disagreement is rejected.
    ConflictingDeferrabilitySpec {
        /// The Rust type name carrying the field.
        model_type_name: String,
        /// The field name.
        field_name: String,
        /// `(deferrable, initially_deferred)` from the first spec.
        first: (bool, bool),
        /// `(deferrable, initially_deferred)` from the second spec.
        second: (bool, bool),
    },
    /// Phase 8β BLOCK-2 fix: a `ForeignKey<T>` / `OneToOneField<T>`
    /// resolves to a proxy descriptor whose `proxy_for` parent is not
    /// registered in the inventory. Proxies never project DDL — the
    /// parent owns the table — so the FK's actual SQL target is the
    /// parent's table. Without the parent registered, the cross-bucket
    /// FK validation pass cannot determine the true target bucket and
    /// the differ would emit a `REFERENCES <parent_table>(id)` clause
    /// against a table no projection step has added.
    ProxyParentNotRegistered {
        /// The bucket containing the FK source column.
        source_bucket: BucketKey,
        /// The source model's table.
        source_table: String,
        /// The source FK column.
        source_column: String,
        /// The proxy type name the FK directly targets.
        proxy_type: String,
        /// The unregistered parent type the proxy points to.
        parent_type: String,
    },
    /// Phase 8β BLOCK-2 fix: a chain of `proxy_for` references forms a
    /// cycle (e.g. `A.proxy_for = B`, `B.proxy_for = A`). The FK
    /// resolution walker would loop forever; reject up front so the
    /// projection bails with an actionable diagnostic. Per the proxy
    /// design (Phase 8β T3), proxies should always terminate at a
    /// concrete (non-proxy) parent — a cycle is a misconfiguration.
    ProxyCycle {
        /// The type at which the cycle was first detected.
        type_name: String,
    },
    /// GH issue #158 — the global
    /// [`crate::relation::registry::ReverseRelationMarker`] inventory
    /// contains at least one `(source, accessor_name)` pair claimed by
    /// markers that disagree on `kind`, `target`, or `via`.
    ///
    /// Surfaced eagerly by [`project_from_inventory`] before any
    /// per-bucket projection work so the diagnostic names the colliding
    /// source, accessor, kind, target, and via metadata rather than an
    /// arbitrary downstream "ambiguous method call" call site. The
    /// carried error value enumerates every offending pair so adopters
    /// can fix the whole registry in one round.
    RelationAccessorCollisions(crate::relation::registry::RelationRegistryError),
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
            ProjectionError::CrossDatabaseForeignKey {
                source_bucket,
                source_table,
                source_column,
                target_bucket,
                target_table,
            } => write!(
                f,
                "cross-database foreign key rejected: `{}.{source_table}.{source_column}` \
                 (database `{}`) references `{target_table}` (database `{}`). \
                 Postgres FK constraints cannot span databases. Use an application-\
                 level join or the outbox pattern instead.",
                source_bucket.app, source_bucket.database, target_bucket.database
            ),
            ProjectionError::ConflictingDeferrabilitySpec {
                model_type_name,
                field_name,
                first,
                second,
            } => write!(
                f,
                "two `DeferrabilitySpec` entries for `{model_type_name}::{field_name}` \
                 disagree: first=(deferrable={}, initially_deferred={}), \
                 second=(deferrable={}, initially_deferred={}). Inventory iteration \
                 order is not deterministic — the macro must emit at most one spec \
                 per `(model_type_name, field_name)`.",
                first.0, first.1, second.0, second.1
            ),
            ProjectionError::ProxyParentNotRegistered {
                source_bucket,
                source_table,
                source_column,
                proxy_type,
                parent_type,
            } => write!(
                f,
                "foreign key `{}.{source_table}.{source_column}` (database `{}`) \
                 targets proxy `{proxy_type}` whose `proxy_for = {parent_type}` parent \
                 is not registered in the inventory. Proxies never project DDL — \
                 the parent owns the table — so the FK target table cannot be \
                 resolved without the parent's descriptor. Register `{parent_type}` \
                 via `#[model(...)]` in a crate that participates in the migration \
                 inventory.",
                source_bucket.app, source_bucket.database
            ),
            ProjectionError::ProxyCycle { type_name } => write!(
                f,
                "proxy chain forms a cycle starting at `{type_name}` — \
                 `proxy_for` references must terminate at a concrete (non-proxy) \
                 parent. Break the cycle by removing one of the `proxy_for` \
                 declarations in the loop."
            ),
            ProjectionError::RelationAccessorCollisions(inner) => write!(
                f,
                "relation-accessor collisions detected before projection \
                 (GH #158); the framework gates `project_from_inventory` on \
                 the global `inventory::iter::<ReverseRelationMarker>` walk \
                 so cross-kind clashes surface with relation metadata \
                 rather than at a downstream `ambiguous method call` site:\n\
                 {inner}"
            ),
        }
    }
}

impl std::error::Error for ProjectionError {}

fn insert_unique<K: Ord, V, E>(
    map: &mut BTreeMap<K, V>,
    key: K,
    value: V,
    on_conflict: impl FnOnce(&V, &V) -> Result<(), E>,
) -> Result<(), E> {
    match map.entry(key) {
        Entry::Vacant(v) => {
            v.insert(value);
            Ok(())
        }
        Entry::Occupied(occ) => on_conflict(occ.get(), &value),
    }
}

/// Project the global descriptor inventory into per-bucket
/// [`AppliedSchema`]s.
///
/// Walks `inventory::iter::<ModelDescriptor>`,
/// `inventory::iter::<EnumDescriptor>`, and [`AppRegistry::all`] —
/// the production entry point. Use [`project_from_iters`] when you
/// need to project from explicit iterables (tests).
///
/// # Pre-projection registry gate (GH #158)
///
/// Before producing any snapshot output this entry point invokes
/// [`crate::relation::registry::validate_global_relation_accessor_registry`]
/// to catch cross-kind reverse / M2M accessor collisions that rustc
/// cannot see (the colliding macros emit different trait suffixes —
/// `…ReverseRelation` vs `…ManyToManyRelation` — so both compile and
/// the clash only manifests at downstream call sites). A failure is
/// wrapped into [`ProjectionError::RelationAccessorCollisions`] before
/// any per-bucket work runs, keeping the diagnostic anchored at the
/// relation registry metadata rather than at the eventual ambiguity
/// error.
/// Custom bootstraps that bypass this entry point can call the
/// validator directly to retain the same gate.
#[allow(clippy::result_large_err)]
pub fn project_from_inventory() -> Result<BTreeMap<BucketKey, AppliedSchema>, ProjectionError> {
    project_from_inventory_with_relation_validator(
        crate::relation::registry::validate_global_relation_accessor_registry,
    )
}

/// Inner half of [`project_from_inventory`] with the relation-registry
/// validator extracted to a closure parameter.
///
/// The production entry point passes
/// [`crate::relation::registry::validate_global_relation_accessor_registry`]
/// directly. Tests inject `|| Ok(())` (clean-registry path) or
/// `|| Err(synthetic_err)` (collision path) to exercise the wrapping
/// without polluting the link-time-collected inventory — submitting a
/// colliding marker via `inventory::submit!` from a test would persist
/// for every other test in the same binary, and the lib's `cargo test`
/// surface should be able to assert both branches.
///
/// `pub(crate)` because the closure type signature is an internal
/// implementation detail; outside callers should keep going through
/// [`project_from_inventory`].
#[allow(clippy::result_large_err)]
pub(crate) fn project_from_inventory_with_relation_validator<F>(
    validator: F,
) -> Result<BTreeMap<BucketKey, AppliedSchema>, ProjectionError>
where
    F: FnOnce() -> Result<(), crate::relation::registry::RelationRegistryError>,
{
    validator().map_err(ProjectionError::RelationAccessorCollisions)?;
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
#[allow(clippy::result_large_err)]
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
    project_from_iters_with_deferrability(
        models,
        enums,
        apps,
        inventory::iter::<DeferrabilitySpec>(),
        generated_at,
    )
}

#[allow(clippy::result_large_err)]
fn project_from_iters_with_deferrability<'a, M, E, A, D>(
    models: M,
    enums: E,
    apps: A,
    deferrability_specs: D,
    generated_at: String,
) -> Result<BTreeMap<BucketKey, AppliedSchema>, ProjectionError>
where
    M: IntoIterator<Item = &'a ModelDescriptor>,
    E: IntoIterator<Item = &'a EnumDescriptor>,
    A: IntoIterator<Item = &'a AppDescriptor>,
    D: IntoIterator<Item = &'static DeferrabilitySpec>,
{
    let models: Vec<&ModelDescriptor> = models.into_iter().collect();
    let apps: Vec<&AppDescriptor> = apps.into_iter().collect();
    // Codex round-7 WARN 6: replace `.collect()` with explicit
    // duplicate detection. Inventory iteration order is not
    // deterministic across builds, so silent last-writer-wins on a
    // disagreeing duplicate would produce non-byte-stable migrations.
    // Idempotent re-emission (same key, same values) is accepted.
    let mut deferrability_by_field: BTreeMap<(&str, &str), (bool, bool)> = BTreeMap::new();
    for spec in deferrability_specs {
        let key = (spec.model_type_name, spec.field_name);
        let value = (spec.deferrable, spec.initially_deferred);
        if let Some(prev) = deferrability_by_field.get(&key)
            && *prev != value
        {
            return Err(ProjectionError::ConflictingDeferrabilitySpec {
                model_type_name: spec.model_type_name.to_string(),
                field_name: spec.field_name.to_string(),
                first: *prev,
                second: value,
            });
        }
        deferrability_by_field.insert(key, value);
    }

    // Build label → AppDescriptor map. Always includes the synthetic
    // global bucket per AppRegistry contract.
    let mut label_to_app: BTreeMap<&str, &AppDescriptor> = BTreeMap::new();
    label_to_app.insert(AppDescriptor::GLOBAL_LABEL, &AppDescriptor::GLOBAL);
    for a in &apps {
        label_to_app.insert(a.label, a);
    }

    // First pass — duplicate type_name detection across the entire
    // inventory (B-1). Reject before doing per-bucket work.
    // Idempotent reinsert (same type_name → same table) is silently
    // accepted; only disagreement raises.
    let mut type_to_table: BTreeMap<&str, &str> = BTreeMap::new();
    for m in &models {
        insert_unique(
            &mut type_to_table,
            m.type_name,
            m.table_name,
            |prev_table, new_table| {
                if prev_table == new_table {
                    Ok(())
                } else {
                    Err(ProjectionError::DuplicateModelTypeName {
                        type_name: m.type_name.to_string(),
                        first_table: (*prev_table).to_string(),
                        second_table: (*new_table).to_string(),
                    })
                }
            },
        )?;
    }

    // Second pass — group models by bucket and build the
    // `type_name → proxy_for_target` map in the same walk. Both maps
    // iterate the full model inventory unconditionally, so a single
    // pass keeps the projection setup linear.
    //
    // The `type_to_bucket` half validates that each model's declared
    // app exists and gives the cross-database FK pass below a fast
    // lookup from a model's `type_name` to its `(database, app)`
    // bucket.
    //
    // The `type_to_proxy_for` half lets the FK pass traverse proxy
    // chains to the concrete parent. Phase 8β BLOCK-2 fix: when an FK
    // targets a proxy, the actual SQL table the FK references is the
    // parent's (proxies are schema-passthrough — never projected).
    // Validating the proxy's bucket would silently accept FKs that
    // point at a non-existent target table when the proxy and parent
    // live in different buckets, and would falsely flag cross-database
    // FKs when the proxy and parent sit in different databases but the
    // source and parent share one.
    let mut bucket_models: BTreeMap<BucketKey, Vec<&ModelDescriptor>> = BTreeMap::new();
    let mut type_to_bucket: BTreeMap<&str, BucketKey> = BTreeMap::new();
    let mut type_to_proxy_for: BTreeMap<&str, &str> = BTreeMap::new();
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
        type_to_bucket.insert(m.type_name, bucket.clone());
        bucket_models.entry(bucket).or_default().push(m);
        if let Some(parent) = m.proxy_for {
            type_to_proxy_for.insert(m.type_name, parent);
        }
    }

    // Cross-database FK validation (Codex T2 review B-3). Postgres
    // FK constraints cannot span databases, so a model in
    // `(main, billing)` referencing a model in
    // `(crud_log, audit)` is structurally invalid. Reject at
    // projection time so the differ never has to reason about
    // cross-database FK transitions.
    //
    // Proxies are skipped on the SOURCE side (the parent's projection
    // already registered the inherited FK columns) and resolved
    // through `type_to_proxy_for` on the TARGET side so we validate
    // against the concrete parent's bucket — see the
    // `type_to_proxy_for` rationale at the second-pass loop above.
    for m in &models {
        if m.proxy_for.is_some() {
            continue;
        }
        let source_label = m.app.unwrap_or(AppDescriptor::GLOBAL_LABEL);
        let source_app = label_to_app[source_label];
        let source_bucket = BucketKey {
            database: source_app.database.to_string(),
            app: source_app.label.to_string(),
        };
        for f in m.fields {
            if f.relation_kind.is_none() {
                continue;
            }
            let Some(target_type) = f.target_type_name else {
                continue;
            };
            // Walk proxy_for chain to the concrete parent. Bounded by
            // a cycle guard at `models.len()` steps — a chain longer
            // than the inventory size must contain a cycle.
            let mut resolved_target: &str = target_type;
            let mut steps = 0usize;
            while let Some(parent) = type_to_proxy_for.get(resolved_target).copied() {
                // The proxy's parent must itself be a registered
                // descriptor — otherwise the FK has no real target
                // table at DDL time. Fail loud rather than silently
                // accept.
                if !type_to_bucket.contains_key(parent) {
                    return Err(ProjectionError::ProxyParentNotRegistered {
                        source_bucket,
                        source_table: m.table_name.to_string(),
                        source_column: f.name.to_string(),
                        proxy_type: resolved_target.to_string(),
                        parent_type: parent.to_string(),
                    });
                }
                resolved_target = parent;
                steps += 1;
                if steps > models.len() {
                    return Err(ProjectionError::ProxyCycle {
                        type_name: target_type.to_string(),
                    });
                }
            }
            let Some(target_bucket) = type_to_bucket.get(resolved_target) else {
                continue; // Unresolved target — falls through to the
                // verbatim ref_table value handled by `project_column`.
            };
            if target_bucket.database != source_bucket.database {
                let target_table = type_to_table
                    .get(resolved_target)
                    .copied()
                    .unwrap_or(resolved_target)
                    .to_string();
                return Err(ProjectionError::CrossDatabaseForeignKey {
                    source_bucket,
                    source_table: m.table_name.to_string(),
                    source_column: f.name.to_string(),
                    target_bucket: target_bucket.clone(),
                    target_table,
                });
            }
        }
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
        insert_unique(
            &mut enum_rust_type_for_pg,
            e.postgres_type,
            e.type_name,
            |prev_rust, new_rust| {
                Err(ProjectionError::DuplicateEnumPostgresType {
                    postgres_type: e.postgres_type.to_string(),
                    first_rust_type: (*prev_rust).to_string(),
                    second_rust_type: (*new_rust).to_string(),
                })
            },
        )?;
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

    // FK column type substitution map — every model's `type_name`
    // mapped to the SQL type its `id` column carries. The migration
    // engine's `CREATE TABLE` emitter inlines `<col> <sql_type>
    // REFERENCES <ref_table> (id)`, and Postgres rejects FK constraints
    // whose source type does not match the target's PK type. The
    // descriptor layer always emits `TEXT` for FK fields (the macro
    // does not look up the target's PK type — it cannot, since the
    // target may live in a separate crate that defines its own
    // descriptor). The projection layer is the canonical place to
    // resolve this since it walks every descriptor in the inventory.
    //
    // Surfaced by Phase 7 T10. Unmapped target type names fall back to
    // the original `f.sql_type` (e.g. when the FK target is in a
    // different `sync_models` call or hasn't been registered yet —
    // `sync_models` itself rejects that case before the projection
    // runs, so the fallback is informational).
    let mut type_to_pk_sql: BTreeMap<&str, String> = BTreeMap::new();
    for m in &models {
        type_to_pk_sql.insert(m.type_name, pk_sql_type_text(&m.pk_type));
    }

    // Build each bucket's AppliedSchema.
    let mut out: BTreeMap<BucketKey, AppliedSchema> = BTreeMap::new();
    for (bucket, ms) in bucket_models {
        let mut tables: BTreeMap<String, TableSchema> = BTreeMap::new();
        let mut indexes: Vec<IndexSchema> = Vec::new();
        for m in &ms {
            // Phase 8β T3.5 — proxy schema-passthrough. Proxies share
            // their parent's table (`#[model(proxy_for = Parent)]`),
            // so emitting DDL here would duplicate the parent's
            // `CREATE TABLE`. Skip projection entirely (table + indexes)
            // so the differ never sees the proxy descriptor as a schema
            // source; index ownership belongs to the parent in v0.1.0.
            // A proxy/parent `table = ...` mismatch is caught at
            // descriptor-lookup time (`T::table_name()`), not here —
            // see `docs/guide/proxy.md` (T5.7).
            if m.proxy_for.is_some() {
                continue;
            }
            let projected =
                project_model(m, &type_to_table, &type_to_pk_sql, &deferrability_by_field);
            insert_unique(
                &mut tables,
                projected.table.clone(),
                projected,
                |existing, duplicate| {
                    Err(ProjectionError::DuplicateTableInBucket {
                        bucket: bucket.clone(),
                        table: duplicate.table.clone(),
                        first_type: existing.table.clone(),
                        second_type: m.type_name.to_string(),
                    })
                },
            )?;
            if m.has_outbox {
                let outbox = project_outbox_table(m, &type_to_pk_sql);
                insert_unique(
                    &mut tables,
                    outbox.table.clone(),
                    outbox,
                    |existing, duplicate| {
                        Err(ProjectionError::DuplicateTableInBucket {
                            bucket: bucket.clone(),
                            table: duplicate.table.clone(),
                            first_type: existing.table.clone(),
                            second_type: format!("{}::outbox", m.type_name),
                        })
                    },
                )?;
                indexes.push(project_outbox_pending_index(m.table_name));
            }
            for idx in m.indexes {
                indexes.push(project_index(idx, m.table_name));
            }
            if let Some(fts) = &m.fts {
                indexes.push(project_fts_index(m.table_name, fts));
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

fn project_model(
    m: &ModelDescriptor,
    type_to_table: &BTreeMap<&str, &str>,
    type_to_pk_sql: &BTreeMap<&str, String>,
    deferrability_by_field: &BTreeMap<(&str, &str), (bool, bool)>,
) -> TableSchema {
    let mut columns: Vec<ColumnSchema> = m
        .fields
        .iter()
        .map(|f| project_column(f, m, type_to_table, type_to_pk_sql, deferrability_by_field))
        .collect();
    if let Some(fts) = &m.fts {
        columns.push(project_fts_column(fts));
    }

    let primary_key = project_primary_key(&m.pk_type);

    let mut exclusion_constraints: Vec<ExclusionConstraintSchema> = m
        .exclusion_constraints
        .iter()
        .map(project_exclusion_constraint)
        .collect();
    exclusion_constraints.sort_by(|a, b| a.name.cmp(&b.name));

    TableSchema {
        app: m.app.map(|s| s.to_string()),
        columns,
        exclusion_constraints,
        fts: m.fts.as_ref().map(project_fts),
        is_through: m.is_through,
        moved_from_app: m.moved_from_app.map(|s| s.to_string()),
        partition: m.partition_by.as_ref().map(project_partition),
        primary_key,
        rationale: m.rationale.map(|s| s.to_string()),
        renamed_from: m.renamed_from.map(|s| s.to_string()),
        rls_enabled: m.tenant_key.is_some(),
        table: m.table_name.to_string(),
        // Phase 8.5 Cluster 4 (djogi#217) — copy adopter
        // `#[model(table_comment = "…")]` from descriptor verbatim.
        // The composer owns single-quote escaping at SQL-emission time.
        table_comment: m.table_comment.map(|s| s.to_string()),
        storage_params: m.storage_params.map(|s| s.to_string()),
        tablespace: m.tablespace.map(|s| s.to_string()),
        tenant_key: m.tenant_key.map(|s| s.to_string()),
    }
}

fn project_fts_column(fts: &FtsDescriptor) -> ColumnSchema {
    ColumnSchema {
        check: None,
        // Phase 8.5 Cluster 4 (djogi#217) — FTS tsvector columns are
        // framework-synthesised; no adopter `#[field(comment)]` flows.
        comment: None,
        default_sql: None,
        foreign_key: None,
        generated: Some(GeneratedColumnSchema {
            expression: fts_generated_expression(fts),
            stored: true,
        }),
        identity: None,
        index_type: None,
        indexed: false,
        max_length: None,
        name: fts.column.to_string(),
        nullable: true,
        on_delete: None,
        outbox_exclude: false,
        rationale: None,
        relation_kind: None,
        renamed_from: None,
        sequence_within: None,
        sql_type: "TSVECTOR".to_string(),
        unique: false,
    }
}

fn fts_generated_expression(fts: &FtsDescriptor) -> String {
    let sources = crate::fts::parse_source_columns(fts.source)
        .expect("macro-emitted FtsDescriptor source columns are validated");
    let source_expr = sources
        .iter()
        .map(|column| quote_ident_expr(column))
        .collect::<Vec<_>>()
        .join(" || ' ' || ");
    format!("to_tsvector('{}', {source_expr})", fts.dictionary)
}

fn quote_ident_expr(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for b in name.as_bytes() {
        if *b == b'"' {
            out.push('"');
            out.push('"');
        } else {
            out.push(*b as char);
        }
    }
    out.push('"');
    out
}

fn project_outbox_table(
    m: &ModelDescriptor,
    type_to_pk_sql: &BTreeMap<&str, String>,
) -> TableSchema {
    let row_id_sql_type = type_to_pk_sql
        .get(m.type_name)
        .cloned()
        .unwrap_or_else(|| pk_sql_type_text(&m.pk_type));
    let table = format!("{}_outbox", m.table_name);

    TableSchema {
        app: m.app.map(|s| s.to_string()),
        columns: vec![
            outbox_column("id", "BIGINT", Some("heerid_next()")),
            outbox_column("row_id", &row_id_sql_type, None),
            outbox_column("action", "TEXT", None)
                .with_check("action IN ('create', 'save', 'delete')"),
            outbox_column("payload", "JSONB", None),
            outbox_column("created_at", "TIMESTAMPTZ", Some("now()")),
            outbox_column("state", "TEXT", Some("'pending'"))
                .with_check("state IN ('pending', 'processing', 'published', 'failed')"),
            outbox_column("leased_until", "TIMESTAMPTZ", None).nullable(),
            outbox_column("retry_count", "INTEGER", Some("0")),
            outbox_column("failed_reason", "TEXT", None).nullable(),
        ],
        exclusion_constraints: Vec::new(),
        fts: None,
        is_through: false,
        moved_from_app: None,
        partition: None,
        primary_key: PrimaryKeySchema {
            columns: vec!["id".to_string()],
            kind: PkKindSchema::HeerId,
        },
        rationale: None,
        renamed_from: None,
        rls_enabled: false,
        table,
        // Phase 8.5 Cluster 4 (djogi#217) — framework-synthesised
        // `<table>_outbox` tables inherit no adopter DDL metadata;
        // comments / storage params / tablespace are model-specific
        // and never copy onto the outbox sibling.
        table_comment: None,
        storage_params: None,
        tablespace: None,
        tenant_key: None,
    }
}

fn project_outbox_pending_index(table: &str) -> IndexSchema {
    let outbox_table = format!("{table}_outbox");
    IndexSchema {
        extension_dependency: None,
        include: Vec::new(),
        index_type: IndexTypeSchema::BTree,
        kind: IndexKindSchema::NonUnique,
        name: outbox_pending_index_name(table),
        nulls_not_distinct: false,
        predicate: Some("state = 'pending'".to_string()),
        requires_out_of_transaction: false,
        table: outbox_table,
        target: IndexTargetSchema::Columns(vec![
            IndexColumnSchema {
                name: "state".to_string(),
                nulls: IndexNullsOrderSchema::Default,
                opclass: None,
                order: IndexOrderSchema::Asc,
            },
            IndexColumnSchema {
                name: "created_at".to_string(),
                nulls: IndexNullsOrderSchema::Default,
                opclass: None,
                order: IndexOrderSchema::Asc,
            },
        ]),
    }
}

fn outbox_pending_index_name(table: &str) -> String {
    let full = format!("{table}_outbox_pending_idx");
    if full.len() <= 63 {
        return full;
    }

    use std::hash::{BuildHasher, BuildHasherDefault, Hasher};
    let mut h =
        BuildHasherDefault::<std::collections::hash_map::DefaultHasher>::default().build_hasher();
    h.write(full.as_bytes());
    let digest = format!("{:08x}", h.finish() as u32);
    let stem: String = full.as_bytes()[..54].iter().map(|b| *b as char).collect();
    format!("{stem}_{digest}")
}

fn outbox_column(name: &str, sql_type: &str, default_sql: Option<&str>) -> ColumnSchema {
    ColumnSchema {
        check: None,
        // Phase 8.5 Cluster 4 (djogi#217) — framework-synthesised
        // outbox columns carry no adopter `#[field(comment)]`; the
        // attribute applies to user-declared fields only.
        comment: None,
        default_sql: default_sql.map(str::to_string),
        foreign_key: None,
        generated: None,
        identity: None,
        index_type: None,
        indexed: false,
        max_length: None,
        name: name.to_string(),
        nullable: false,
        on_delete: None,
        outbox_exclude: false,
        rationale: None,
        relation_kind: None,
        renamed_from: None,
        sequence_within: None,
        sql_type: sql_type.to_string(),
        unique: false,
    }
}

trait OutboxColumnExt {
    fn nullable(self) -> Self;
    fn with_check(self, check: &str) -> Self;
}

impl OutboxColumnExt for ColumnSchema {
    fn nullable(mut self) -> Self {
        self.nullable = true;
        self
    }

    fn with_check(mut self, check: &str) -> Self {
        self.check = Some(check.to_string());
        self
    }
}

fn project_exclusion_constraint(spec: &ExclusionConstraintSpec) -> ExclusionConstraintSchema {
    ExclusionConstraintSchema {
        name: spec.name.to_string(),
        using: spec.using.to_string(),
        elements: spec
            .elements
            .iter()
            .map(project_exclusion_element)
            .collect(),
        where_clause: spec.where_clause.map(|s| s.to_string()),
        deferrable: spec.deferrable,
        initially_deferred: spec.initially_deferred,
    }
}

fn project_exclusion_element(elem: &ExclusionElement) -> ExclusionElementSchema {
    ExclusionElementSchema {
        expr: elem.expr.to_string(),
        with_operator: elem.with_operator.to_string(),
    }
}

fn project_generated_column(spec: &GeneratedColumnSpec) -> GeneratedColumnSchema {
    GeneratedColumnSchema {
        expression: spec.expression.to_string(),
        stored: spec.stored,
    }
}

/// Render a column-level CHECK expression bounding the widened column
/// to the Rust source type's representable range.
///
/// Returns `None` for columns whose Rust type maps identity-width to
/// the Postgres column type (`i16`, `i32`, `i64`, `bool`, `String`,
/// `f32`, `f64`, ...) — the column type already enforces the range,
/// and a redundant CHECK would inflate every snapshot for no safety
/// win.
///
/// **Active arms (djogi#187 — temporal year bounds).** Each temporal
/// `FieldSqlType` variant has exactly one Rust source type that lowers
/// to it (`time::Date` → `Date`, `time::OffsetDateTime` → `Timestamptz`),
/// so dispatch on `FieldSqlType` alone is unambiguous and the CHECK
/// reaches `project_column` directly. The bound shape:
///
/// | Rust                     | Postgres column | CHECK expression                                      |
/// |--------------------------|-----------------|-------------------------------------------------------|
/// | `time::Date`             | `DATE`          | `<col> <= DATE '9999-12-31'`                          |
/// | `time::OffsetDateTime`   | `TIMESTAMPTZ`   | `<col> <= TIMESTAMPTZ '9999-12-31 23:59:59.999999+00'`|
///
/// **One-sided upper bound by design.** The `time` crate's default
/// year range is ISO 8601 -9999 to +9999. Postgres's DATE / TIMESTAMP
/// types accept years far outside the upper bound (DATE up to 5874897
/// AD; TIMESTAMP up to 294276 AD), so the upper-bound CHECK is the
/// one that actually does work — it rejects Postgres-valid but
/// `time::Date`-OOB INSERTs that would otherwise corrupt typed reads.
///
/// The lower bound is intentionally omitted because Postgres's own
/// date input parser already rejects every value `time::Date` cannot
/// represent: ISO year -9999 = 10000 BC, but Postgres's earliest
/// representable date is 4713 BC (= ISO year -4712). Any literal in
/// ISO years -9999 to -4713 (= 10000 BC to 4714 BC) fails Postgres's
/// own type-validation pass and never reaches the CHECK constraint.
/// A redundant lower-bound CHECK at `4713-01-01 BC` would add no
/// safety and inflate every DATE / TIMESTAMPTZ column DDL with a 30+
/// byte clause Postgres cannot test. See
/// `docs/spec/decisions.md` "Type-derived CHECK projection" for the
/// rationale.
///
/// Adopters who enable the `time/large-dates` feature flag widen the
/// representable range to ±999_999; the CHECK projected here remains
/// at the default ±9999 by design — `time::Date::MAX_YEAR` is a
/// compile-time constant the projection layer cannot inspect without
/// leaking the feature flag into the descriptor surface.
///
/// **Live arms (djogi#190 — integer widening).** The integer arms
/// (`i8 / u8 / u16 / u32 / u64`) are now gated on the
/// `rust_source_type` discriminator introduced by djogi#190 on
/// `FieldDescriptor`. Only columns whose descriptor carries a
/// `Some(RustSourceType::*)` value receive a range CHECK; every other
/// `SmallInt` / `Integer` / `BigInt` / `NumericPrecision` column
/// (e.g. an `i16` field lowering to `SmallInt`, or an `i64` field
/// lowering to `BigInt`) keeps `None` so no spurious CHECK is emitted.
///
/// The expression references the column by its quoted name so
/// identifiers using reserved words round-trip cleanly through
/// Postgres parsing. The descriptor's `name` field is the user's Rust
/// ident with `r#` stripped, which matches the column-name convention
/// the rest of the projection / SQL emitter uses.
fn field_type_check(
    sql_type: &crate::descriptor::FieldSqlType,
    rust_source_type: Option<RustSourceType>,
    column_name: &str,
) -> Option<String> {
    use crate::descriptor::{FieldSqlType, RangeSubtypeKind};

    let qcol = quote_ident_for_check(column_name);
    match sql_type {
        // ── djogi#187 — temporal year upper bound ────────────────────
        //
        // `time::Date` / `time::OffsetDateTime` cap at ISO year 9999
        // by default. Postgres `DATE` / `TIMESTAMPTZ` accept much
        // higher years (`DATE` up to 5874897 AD; `TIMESTAMPTZ` up to
        // 294276 AD). External writers (raw SQL migration, BI tool,
        // sister application) can land a row whose year exceeds the
        // time crate's MAX. The next typed `SELECT` decoding via
        // `row.try_get::<Date>` / `row.try_get::<OffsetDateTime>`
        // fails with `DjogiError::Decode`, and a single bad row
        // poisons all subsequent reads through the typed surface.
        //
        // Project a one-sided upper-bound CHECK so Postgres rejects
        // OOB-upper writes at the DB layer rather than letting them
        // land and surface as decode failures on the read side. The
        // lower bound is omitted because Postgres's own date input
        // parser rejects every value `time::Date` cannot represent
        // (Postgres's MIN is 4713 BC; `time::Date`'s MIN is 10000 BC
        // — values in ISO years -9999 to -4713 are unreachable
        // through Postgres regardless of CHECK). See the doc comment
        // above.
        //
        // The upper-bound `TIMESTAMP` literal includes microsecond
        // resolution so `OffsetDateTime::new(..., 23, 59, 59, 999_999)`
        // round-trips identically and CHECK-equals against the literal.
        FieldSqlType::Date => Some(date_range_expr(&qcol)),
        FieldSqlType::Timestamptz => {
            // Emit an explicit UTC timestamptz literal so the CHECK is
            // timezone-invariant. Using `TIMESTAMP '...'` (without TZ) against
            // a TIMESTAMPTZ column makes Postgres interpret the literal in the
            // session timezone, which shifts the effective upper bound by the
            // session UTC offset. `TIMESTAMPTZ '...+00'` is always interpreted
            // as UTC regardless of session timezone, matching the
            // `time::OffsetDateTime` MAX of `9999-12-31 23:59:59.999999 UTC`.
            Some(timestamptz_range_expr(&qcol))
        }
        // ── djogi#190 — integer widening (live, gated on rust_source_type) ──
        //
        // The discriminator ensures the CHECK fires ONLY for the narrow /
        // unsigned types (i8/u8/u16/u32/u64), not for every adopter `i16` /
        // `i64` column that shares the same `FieldSqlType` variant.
        //
        // Range bounds follow the Rust type's representable range:
        //
        //   i8  → SMALLINT : -128..=127
        //   u8  → SMALLINT : 0..=255
        //   u16 → INTEGER  : 0..=65535
        //   u32 → BIGINT   : 0..=4294967295
        //   u64 → NUMERIC  : 0..=18446744073709551615 AND col = trunc(col)
        //
        // Two-sided CHECKs (lower AND upper) are emitted for all five types:
        // the lower bound guards against negative values written by an
        // external writer; the upper bound guards against values exceeding
        // the Rust type's MAX.
        //
        // **u64 integrality check**: u64 uses bare `NUMERIC` (no precision/scale).
        // Unlike `NUMERIC(20, 0)`, bare NUMERIC does NOT round fractional inputs —
        // it stores exactly what is given. The `col = trunc(col)` predicate in the
        // CHECK rejects any stored value whose fractional part is non-zero (e.g.
        // a raw `INSERT … NUMERIC '1.5'`). The decode path (`decode_u64_from_decimal`)
        // also enforces the same integrality guard on the Rust side, but the DB-level
        // CHECK prevents the value from landing in the first place.
        //
        // `None` for direct-mapped integer columns without a
        // `rust_source_type` discriminator (`i16 → SMALLINT`,
        // `i32 → INTEGER`, `i64 → BIGINT`); the Postgres column type
        // already enforces the relevant range. Adopter `Decimal → NUMERIC`
        // columns reach the `FieldSqlType::Numeric` arm below — they carry
        // `Some(RustSourceType::Decimal)` and project a structural CHECK
        // (djogi#188), not None.
        FieldSqlType::SmallInt => match rust_source_type {
            Some(RustSourceType::I8) => Some(format!("{qcol} >= -128 AND {qcol} <= 127")),
            Some(RustSourceType::U8) => Some(format!("{qcol} >= 0 AND {qcol} <= 255")),
            _ => None,
        },
        FieldSqlType::Integer => match rust_source_type {
            Some(RustSourceType::U16) => Some(format!("{qcol} >= 0 AND {qcol} <= 65535")),
            _ => None,
        },
        FieldSqlType::BigInt => match rust_source_type {
            Some(RustSourceType::U32) => Some(format!("{qcol} >= 0 AND {qcol} <= 4294967295")),
            _ => None,
        },
        // ── djogi#190 (u64) and djogi#188 (Decimal) share the bare-NUMERIC
        //    column type but project distinct CHECK shapes. The
        //    `rust_source_type` discriminator routes each Rust source to
        //    the right CHECK. ────────────────────────────────────────────
        //
        // **u64** — bare NUMERIC with range + integrality CHECK.
        // The integrality clause (`col = trunc(col)`) is the critical
        // addition over the old NUMERIC(20, 0) design: bare NUMERIC
        // preserves fractional inputs unchanged, so the CHECK must
        // explicitly reject them. `trunc()` is the standard Postgres
        // function for truncating a NUMERIC toward zero.
        //
        // **Decimal (djogi#188)** — bare NUMERIC with a structural CHECK
        // bounding the value to `rust_decimal::Decimal`'s **exact
        // representable** range. The CHECK enforces no-silent-loss
        // semantics: Postgres rejects any write that the typed Rust
        // path would otherwise rescale, round, or fail to fit.
        //
        //   * `scale(col) IS NOT NULL` — Postgres NUMERIC admits three
        //     non-finite special values (`NaN`, `Infinity`, `-Infinity`)
        //     that `rust_decimal::Decimal` cannot represent at all. The
        //     `pg_catalog.scale()` function returns NULL for every
        //     non-finite NUMERIC (NaN since PG 12, ±Infinity since
        //     PG 14, both covered by Djogi's PG 18+ baseline), and
        //     `IS NOT NULL` collapses that to a concrete FALSE so the
        //     CHECK fails on those inputs rather than NULL-propagating
        //     to PASS. Without this guard the later `scale <= 28` and
        //     coefficient clauses would NULL-propagate (`NULL <= 28`
        //     is NULL) and CHECK semantics (`NULL` treated as
        //     satisfied) would silently admit `'NaN'::numeric` and
        //     friends — a typed `Decimal::from_sql` decode would then
        //     fail with `DjogiError::Decode` on the read side. The
        //     scalar `FieldSqlType::Numeric` arm wraps the whole
        //     conjunction with `({qcol}) IS NULL OR (...)` so this
        //     guard does not also reject SQL NULL on nullable Decimal
        //     columns; for `NUMRANGE` endpoints the equivalent NULL
        //     pass-through is provided by `range_endpoint_checks`.
        //   * `scale(col) <= 28` — `rust_decimal::Decimal` carries
        //     5 scale bits inside an i32 word, capping representable
        //     scale at `Decimal::MAX_SCALE = 28`. The pinned
        //     rust_decimal Postgres `FromSql` impl (see
        //     `rust_decimal::postgres::common::checked_from_postgres`)
        //     does **not** reject scale > 28 outright — it silently
        //     rescales the incoming NUMERIC to scale 28 with rounding
        //     (`result.rescale((scale as u32).min(Self::MAX_SCALE))`).
        //     This CHECK rejects such writes at the DB layer so the
        //     value Postgres holds matches the value Rust will decode,
        //     bit for bit, with no silent precision loss on the way in.
        //   * `abs(col) * power(10::numeric, scale(col)) <= 79228162514264337593543950335`
        //     — the rust_decimal coefficient is a 96-bit unsigned mantissa,
        //     i.e. `coefficient <= 2^96 - 1 = 79_228_162_514_264_337_593_543_950_335`.
        //     For any NUMERIC `col` with scale `s`, the coefficient is
        //     `|col| * 10^s`; the CHECK enforces this stays within the
        //     96-bit envelope. Values that overflow this envelope cause
        //     `checked_from_postgres` to return `None` (a hard decode
        //     failure surfaced as `DjogiError::Decode`) — this CHECK
        //     rejects them at the DB layer before they can poison a
        //     typed read.
        //
        // The three-clause shape rejects every kind of unrepresentable value:
        //   - A 100-digit integer at scale 0 → coefficient > 2^96-1 → rejected.
        //   - A value with scale 50 → scale check fails → rejected
        //     (preventing the silent rescale-and-round path).
        //   - A `NaN` / `Infinity` / `-Infinity` literal → `scale()` returns
        //     NULL → `scale(col) IS NOT NULL` evaluates to FALSE → rejected
        //     (preventing the special-value-poisons-decode path).
        // For valid rust_decimal values (e.g. `49.99` with scale 2,
        // coefficient `4999`), all three clauses pass.
        //
        // Postgres semantics: CHECK constraints treat NULL as satisfied,
        // so nullable Decimal columns work without modification. The
        // `power()` and `scale()` calls are stable Postgres functions
        // (no extensions required); per-row overhead is one NUMERIC
        // arithmetic operation plus one function call. The umbrella
        // issue #185 records a per-write budget target for the family
        // but is still open — a measured-µs claim against that budget
        // is left to #185's benchmark workstream rather than asserted
        // here.
        //
        // The performance trade-off versus a column-side `NUMERIC(P, S)`
        // was deliberate (see `docs/spec/decisions.md` "Decimal precision
        // and scale projection (djogi#188)"): `NUMERIC(28, 28)` admits
        // ±9.x (one integer digit) and rejects ordinary adopter values
        // like `49.99`; `NUMERIC(29, 14)` or similar arbitrary
        // precision/scale defaults silently round adopter writes —
        // worse than rejecting them. Bare NUMERIC with a structural
        // CHECK preserves the adopter's full precision/scale choice
        // and rejects only values that fall outside rust_decimal's
        // exact-representable domain — i.e., values that would either
        // be silently rescaled / rounded on decode (scale > 28) or
        // outright fail to decode (coefficient > 2^96 - 1).
        //
        // No `rust_source_type` discriminator (the fall-through `_` arm)
        // means a bare-NUMERIC column whose Rust source is neither `u64`
        // nor `Decimal` — i.e., a user-defined type with
        // `DjogiSqlType::SQL_TYPE = "NUMERIC"`. Those columns carry no
        // type-derived CHECK because the framework does not know the
        // representable range of an adopter scalar type.
        FieldSqlType::Numeric => match rust_source_type {
            Some(RustSourceType::U64) => Some(format!(
                "{qcol} >= 0 AND {qcol} <= 18446744073709551615 AND {qcol} = trunc({qcol})"
            )),
            // Wrap the scalar Decimal representability check with an explicit
            // NULL pass-through. The inner `decimal_repr_expr` carries a
            // `scale(<col>) IS NOT NULL` guard that rejects PostgreSQL NUMERIC
            // special values (`NaN`, `Infinity`, `-Infinity`) — `scale()` is
            // defined to return NULL for those, and `IS NOT NULL` evaluates to
            // FALSE for a NULL input (never NULL itself), so a bare guard would
            // also reject SQL NULL on nullable columns. The `(<col>) IS NULL OR`
            // outer wrap restores Postgres CHECK's standard "NULL satisfies
            // the constraint" behaviour for nullable Decimal columns.
            Some(RustSourceType::Decimal) => Some(format!(
                "({qcol}) IS NULL OR ({})",
                decimal_repr_expr(&qcol)
            )),
            _ => None,
        },
        FieldSqlType::TimestamptzArray => {
            Some(array_all_bound_checks(&qcol, TIMESTAMPTZ_ARRAY_MAX_BOUND))
        }
        FieldSqlType::DateArray => Some(array_all_bound_checks(&qcol, DATE_ARRAY_MAX_BOUND)),
        FieldSqlType::NumericArray => Some(numeric_array_is_rust_decimal_check(&qcol)),
        FieldSqlType::Range { subtype } => match subtype {
            // Int4RANGE / INT4RANGE and INT8RANGE / INT8RANGE are
            // identity-mapped by their Postgres column types in Rust
            // (`i32` / `i64`) so they intentionally return None.
            RangeSubtypeKind::Int4 | RangeSubtypeKind::Int8 => None,
            // Apply the scalar bound logic to both finite bounds; unbounded,
            // empty, and NULL ranges remain exempted by the endpoint
            // `IS NULL` guard.
            RangeSubtypeKind::Num => Some(range_endpoint_checks(&qcol, decimal_repr_expr)),
            RangeSubtypeKind::Tstz => Some(range_endpoint_checks(&qcol, timestamptz_range_expr)),
            RangeSubtypeKind::Date => Some(range_endpoint_checks(&qcol, date_range_expr)),
        },
        // All other `FieldSqlType` variants (`Text`, `Real`,
        // `DoublePrecision`, `Boolean`, `Uuid`, `Jsonb`,
        // arrays, `Citext`, `Geography`, `Custom`, and every
        // `NumericPrecision { .. }` instance — djogi#188 ships
        // `rust_decimal::Decimal` as bare `Numeric` + structural CHECK,
        // not as `NumericPrecision`) carry their own type bounds via
        // the column type itself; no Rust-derived CHECK applies. Future
        // families plug into this same match without reshaping the
        // helper signature.
        _ => None,
    }
}

fn date_range_expr(column_expr: &str) -> String {
    format!("{column_expr} <= DATE '9999-12-31'")
}

fn timestamptz_range_expr(column_expr: &str) -> String {
    format!("{column_expr} <= TIMESTAMPTZ '9999-12-31 23:59:59.999999+00'")
}

/// Inner representability predicate for a NUMERIC expression that
/// resolves to a `rust_decimal::Decimal`-storable value.
///
/// The leading `scale({column_expr}) IS NOT NULL` clause rejects the
/// PostgreSQL NUMERIC special values `NaN`, `Infinity`, and
/// `-Infinity` — `pg_catalog.scale()` is defined to return NULL for
/// non-finite NUMERICs (NaN since PG 12, ±Infinity since PG 14, both
/// covered by Djogi's PG 18+ baseline), and `IS NOT NULL` collapses
/// that to a concrete FALSE so the CHECK fails on those inputs rather
/// than NULL-propagating to PASS. Regular finite NUMERICs continue
/// through the existing scale / coefficient bounds.
///
/// **Callers are responsible for the NULL pass-through.** This helper
/// returns a bare conjunction with no `<col> IS NULL OR` outer wrap:
/// the scalar `FieldSqlType::Numeric` arm wraps the result with
/// `({qcol}) IS NULL OR (...)`, and `range_endpoint_checks` already
/// wraps `lower(...)` / `upper(...)` bound checks with their own
/// `IS NULL OR` so unbounded / empty / NULL ranges short-circuit.
/// Calling this helper directly on a column expression without one of
/// those wraps would reject SQL NULL alongside the special values,
/// because `scale(NULL) IS NOT NULL` evaluates to FALSE.
fn decimal_repr_expr(column_expr: &str) -> String {
    format!(
        "scale({column_expr}) IS NOT NULL AND \
         scale({column_expr}) <= 28 AND \
         abs({column_expr}) * power(10::numeric, scale({column_expr})) <= 79228162514264337593543950335"
    )
}

fn range_endpoint_checks(range_column: &str, bound_check: fn(&str) -> String) -> String {
    let lower_endpoint = format!("lower({range_column})");
    let upper_endpoint = format!("upper({range_column})");
    let lower = format!(
        "{lower_endpoint} IS NULL OR ({})",
        bound_check(&lower_endpoint)
    );
    let upper = format!(
        "{upper_endpoint} IS NULL OR ({})",
        bound_check(&upper_endpoint)
    );
    let lower = format!("({lower})");
    let upper = format!("({upper})");
    format!("{lower} AND {upper}")
}

const DATE_ARRAY_MAX_BOUND: &str = "DATE '9999-12-31'";

const TIMESTAMPTZ_ARRAY_MAX_BOUND: &str = "TIMESTAMPTZ '9999-12-31 23:59:59.999999+00'";

fn array_all_bound_checks(array_column: &str, upper_bound: &str) -> String {
    format!("({array_column} IS NULL OR ({upper_bound} >= ALL({array_column})))")
}

fn numeric_array_is_rust_decimal_check(array_column: &str) -> String {
    format!(
        "{array_column} IS NULL OR djogi.__djogi_numeric_array_is_rust_decimal_v1({array_column})"
    )
}

/// Combine a type-derived CHECK with an adopter `#[field(check = "...")]`
/// expression into a single constraint slot.
///
/// Both forms produce an `Option<String>`; the combination rules:
///
/// - Neither present → `None` (no CHECK constraint).
/// - Only one present → the present one verbatim (no extra parentheses).
/// - Both present → `({type-derived}) AND ({adopter})` — single SQL
///   expression, both clauses must pass.
///
/// The single constraint slot keeps the ADD / DROP / AMEND lifecycle in
/// the differ unchanged: a column has at most one CHECK at
/// `<table>_<column>_check`. Constraint name uniqueness is guaranteed by
/// `migrate/sql.rs::check_constraint_name`. The combined-expression
/// approach loses a small amount of fault-diagnostic granularity (a CHECK
/// violation surfaces the whole `(A) AND (B)` expression rather than
/// pinpointing which clause failed), but Postgres includes the full
/// expression text in the error message so adopters can still tell the
/// type bound from the adopter bound on inspection.
///
/// Defensive normalisation: both inputs are `trim()`'d to avoid
/// `"(expr1 ) AND ( expr2)"` whitespace artefacts in snapshot output.
/// The differ compares CHECK expressions by string equality, so any
/// drift in whitespace would emit a spurious AMEND on every compose.
fn combine_check_expressions(
    type_derived: Option<String>,
    adopter: Option<&str>,
) -> Option<String> {
    match (type_derived, adopter) {
        (None, None) => None,
        (Some(t), None) => Some(t),
        (None, Some(a)) => Some(a.trim().to_string()),
        (Some(t), Some(a)) => Some(format!("({}) AND ({})", t.trim(), a.trim())),
    }
}

/// Local quoter for CHECK expressions — duplicates `quote_ident` from
/// `migrate/sql.rs` because the projection layer cannot depend back
/// on the SQL emitter. The two implementations stay byte-identical;
/// any change to one must update the other.
///
/// Wraps the identifier in double quotes and doubles any embedded
/// double quote (the Postgres rule). Matches the convention every other
/// SQL emitter in this crate uses for column references.
fn quote_ident_for_check(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for byte in name.bytes() {
        if byte == b'"' {
            out.push('"');
            out.push('"');
        } else {
            out.push(byte as char);
        }
    }
    out.push('"');
    out
}

fn project_column(
    f: &FieldDescriptor,
    parent: &ModelDescriptor,
    type_to_table: &BTreeMap<&str, &str>,
    type_to_pk_sql: &BTreeMap<&str, String>,
    deferrability_by_field: &BTreeMap<(&str, &str), (bool, bool)>,
) -> ColumnSchema {
    let projected_on_delete = if f.relation_kind.is_some() {
        Some(project_on_delete(f.on_delete.unwrap_or(OnDelete::Restrict)))
    } else {
        None
    };

    let foreign_key = if f.relation_kind.is_some() {
        f.target_type_name.map(|target| {
            let (deferrable, initially_deferred) = deferrability_by_field
                .get(&(parent.type_name, f.name))
                .copied()
                .unwrap_or((false, false));
            let ref_table = type_to_table
                .get(target)
                .copied()
                .unwrap_or(target)
                .to_string();
            ForeignKeySchema {
                // FK and column carry the same cascade — the
                // projection is the single source feeding both. Once
                // a future descriptor change moves `on_delete` off
                // the column, this is the only field that has to
                // survive.
                on_delete: projected_on_delete.unwrap_or(OnDeleteSchema::Restrict),
                ref_column: "id".to_string(),
                ref_table,
                deferrable,
                initially_deferred,
            }
        })
    } else {
        None
    };

    let default_sql = if f.name == "id" {
        pk_default_sql(&parent.pk_type)
    } else if (f.name == "created_at" || f.name == "updated_at")
        && matches!(f.sql_type, crate::descriptor::FieldSqlType::Timestamptz)
    {
        // Framework-injected timestamp columns get `DEFAULT now()` so
        // INSERTs without explicit values pick up server time. The
        // descriptor layer does not carry per-field default expressions
        // (every field shares one nullable shape), so the projection
        // owns this rule for the two framework cols. Phase 1's
        // hand-written CREATE TABLE statements (`tests/integration/
        // migrations/phase3/*.sql` and friends) have always used this
        // shape — the migration engine produces the same DDL by
        // recording the default here.
        //
        // Surfaced by Phase 7 T10 (`#[djogi_test(sync_models = [...])]`)
        // — without this, every sync_models'd table rejected its first
        // INSERT with `null value in column "created_at"` because the
        // typed `Model::create` path leaves `created_at` blank for the
        // DB to populate via the column DEFAULT.
        Some("now()".to_string())
    } else {
        None
    };

    // FK column SQL type — substitute the target's PK SQL type when
    // this column carries a relation. The descriptor layer cannot do
    // this lookup (the macro emits each model's descriptor in
    // isolation; the FK target may live in a separate crate), so the
    // projection is the canonical resolution site. Unrelated columns
    // pass through `f.sql_type` verbatim.
    let sql_type = if f.relation_kind.is_some()
        && let Some(target) = f.target_type_name
        && let Some(pk_sql) = type_to_pk_sql.get(target)
    {
        pk_sql.clone()
    } else {
        f.sql_type.to_string()
    };

    // Serial PK id-column identity clause (#86): the framework's typed
    // surface declares `pk = Serial` adopters get an auto-incrementing
    // INTEGER primary key (per `docs/spec/primary-keys.md`). The
    // descriptor emits `INTEGER` as the id field's sql_type with no
    // sequencing clause, and `pk_default_sql(Serial)` returns `None`
    // because IDENTITY is not a DEFAULT expression — it has its own
    // ALTER COLUMN ADD/DROP IDENTITY syntax for migrations.
    //
    // Storing the IDENTITY intent in a dedicated `identity` field on
    // ColumnSchema (rather than inlining the clause into `sql_type`)
    // means:
    //   1. The IR shape is correct — IDENTITY is not part of the
    //      column type; it has its own ALTER COLUMN ADD/DROP IDENTITY
    //      syntax. Inlining into sql_type would route through
    //      `ColumnChange::ChangeType` and emit
    //      `ALTER COLUMN id TYPE INTEGER GENERATED BY DEFAULT AS IDENTITY`
    //      which is invalid SQL.
    //   2. Snapshot round-trip stays clean — the sql_type comparison
    //      is "INTEGER" both before and after the fix; only the new
    //      `identity` field changes.
    //   3. CREATE TABLE DDL emission renders the IDENTITY clause via
    //      the dedicated branch in `migrate/sql.rs::push_column_inline`,
    //      producing valid Postgres syntax.
    //
    // Note: the differ at `migrate/diff.rs::diff_column_alter` does
    // NOT yet compare the `identity` field. Snapshot-upgrade scenarios
    // (e.g. an adopter who ran compose before this fix and now has
    // `identity: None` on disk while a fresh projection produces
    // `identity: Some(ByDefault)`) won't trigger the
    // `ALTER COLUMN id ADD GENERATED BY DEFAULT AS IDENTITY` migration
    // — the diff is silent on the identity field. Pre-publish, adopters
    // reset and recompose. Tracked as anchored follow-up #93 with
    // `ColumnChange::SetIdentity { from, to }` IR variant + lowering
    // pipeline mapped out.
    //
    // FK columns *referencing* a Serial PK must NOT get the IDENTITY
    // clause — sequence ownership lives on the parent's PK column,
    // not its references. The guard `f.name == "id" && parent.pk_type
    // == Serial` is airtight: PkType::Composite and PkType::Custom
    // are distinct enum variants; the FK-substitution path above
    // produces sql_type = "INTEGER" without IDENTITY.
    let identity = if f.name == "id" && matches!(parent.pk_type, PkType::Serial) {
        Some(crate::migrate::schema::IdentityKindSchema::ByDefault)
    } else {
        None
    };

    // Type-derived CHECK projection (djogi#186 contract; djogi#187 for
    // temporal types; djogi#190 for integer widening; djogi#188 for
    // Decimal structural bounds; djogi#105 for adopter
    // `#[field(check)]` expressions).
    //
    // The contract:
    //
    //   * `field_type_check` dispatches on the descriptor's
    //     `FieldSqlType` + `rust_source_type` to emit a type-derived
    //     CHECK for widened or structurally-bounded columns whose
    //     Postgres column type accepts values outside the Rust source
    //     type's representable range.
    //   * Adopter-supplied `#[field(check = "<expr>")]` flows through
    //     `f.check_sql` and is combined with the type-derived CHECK
    //     via logical `AND` so a single constraint slot
    //     (`<table>_<column>_check`) carries both. The differ's ADD /
    //     DROP / AMEND lifecycle stays unchanged.
    //   * For non-FK columns we call the helper; non-`None` results
    //     reach `ColumnSchema.check`, the SQL emitter inlines them on
    //     CREATE TABLE and the differ emits `ColumnChange::SetCheck`
    //     for ADD / DROP / AMEND lifecycles.
    //   * FK columns inherit their type from the parent's PK, which is
    //     always identity-width (BIGINT for HeerId, UUID for RanjId).
    //     The Rust-derived CHECK doesn't apply, so the type-derived
    //     half is hard-coded `None`. Adopter `#[field(check)]` on an
    //     FK column is still honoured — the adopter may want a domain
    //     invariant on the FK column itself (e.g., `owner_id > 0`),
    //     and there is no structural reason to forbid it.
    //
    // **Live arms inside `field_type_check`:**
    //   - djogi#187: `Date` / `Timestamptz` → year ±9999 upper-bound
    //     CHECK (unconditional — FieldSqlType alone disambiguates).
    //   - djogi#190: `SmallInt` / `Integer` / `BigInt` / `Numeric`
    //     → range CHECK (+ integrality for u64) gated on the
    //     `rust_source_type` discriminator. Direct-mapped types
    //     (`i16 → SmallInt`, `i64 → BigInt`) have `rust_source_type: None`
    //     and keep `check: None` so no spurious CHECK fires.
    //   - djogi#188: `Numeric` + `Some(RustSourceType::Decimal)` →
    //     structural CHECK enforcing rust_decimal's 96-bit mantissa /
    //     scale-≤-28 representable range.
    let type_derived_check: Option<String> = if foreign_key.is_some() {
        // FK columns inherit their type from the parent's PK (BIGINT for
        // HeerId, UUID for RanjId). The Rust-derived CHECK doesn't apply.
        None
    } else {
        field_type_check(&f.sql_type, f.rust_source_type, f.name)
    };
    let check: Option<String> = combine_check_expressions(type_derived_check, f.check_sql);

    ColumnSchema {
        check,
        // Phase 8.5 Cluster 4 (djogi#217) — copy adopter
        // `#[field(comment = "…")]` from descriptor verbatim. The
        // composer owns single-quote escaping at SQL-emission time.
        comment: f.comment.map(|s| s.to_string()),
        default_sql,
        foreign_key,
        generated: f.generated.as_ref().map(project_generated_column),
        identity,
        index_type: f.index_type.map(project_index_type),
        indexed: f.indexed,
        max_length: f.max_length,
        name: f.name.to_string(),
        nullable: f.nullable,
        on_delete: projected_on_delete,
        outbox_exclude: f.outbox_exclude,
        rationale: f.rationale.map(|s| s.to_string()),
        relation_kind: f.relation_kind.map(project_relation_kind),
        renamed_from: f.renamed_from.map(|s| s.to_string()),
        sequence_within: f.sequence_within.map(|s| s.to_string()),
        sql_type,
        unique: f.unique,
    }
}

/// Render the SQL type text for a model's PK as it appears on FK
/// columns referencing that model.
///
/// `HeerId` / `HeerIdRecencyBiased` → `BIGINT`,
/// `RanjId` / `RanjIdRecencyBiased` → `UUID`,
/// `Serial` → `INTEGER`,
/// `Custom { sql_type, .. }` → that text verbatim,
/// `Composite` / `None` → `TEXT` (placeholder; FK references against
///   composite or no-PK tables are rejected upstream by the descriptor
///   contract — the placeholder lets the projection complete instead
///   of panicking, and the broken DDL surfaces at apply time).
fn pk_sql_type_text(pk: &PkType) -> String {
    match pk {
        PkType::HeerId | PkType::HeerIdDesc => "BIGINT".to_string(),
        PkType::RanjId | PkType::RanjIdDesc => "UUID".to_string(),
        PkType::Serial => "INTEGER".to_string(),
        PkType::Custom(c) => c.sql_type.to_string(),
        // A model with no PK or a composite PK cannot legitimately
        // be the target of an FK (Postgres requires the referenced
        // column to be a single-column unique index). The descriptor
        // layer catches this earlier; the placeholder here is for
        // forward-compatibility with future PK shapes.
        PkType::Composite(_) | PkType::None => "TEXT".to_string(),
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

pub(crate) fn pk_default_sql(pk: &PkType) -> Option<String> {
    // Function names match HeerRanjId 0.3.x's installed schema:
    // `heerid_next()` (asc HeerId) and `ranjid_next()` (asc RanjId)
    // ship from `generate_heerid.sql` / `generate_ranjid.sql`;
    // `heerid_next_desc()` and `ranjid_next_desc()` ship from
    // `desc_generators.sql`. The earlier `generate_id*` /
    // `generate_ranj_id*` names do not exist in the installed schema,
    // so any migration touching these PK kinds would have failed at
    // apply time.
    match pk {
        PkType::HeerId => Some("heerid_next()".to_string()),
        PkType::HeerIdDesc => Some("heerid_next_desc()".to_string()),
        PkType::RanjId => Some("ranjid_next()".to_string()),
        PkType::RanjIdDesc => Some("ranjid_next_desc()".to_string()),
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

fn project_fts_index(table: &str, fts: &FtsDescriptor) -> IndexSchema {
    IndexSchema {
        extension_dependency: None,
        include: Vec::new(),
        index_type: IndexTypeSchema::Gin,
        kind: IndexKindSchema::NonUnique,
        name: fts_index_name(table, fts.column),
        nulls_not_distinct: false,
        predicate: None,
        requires_out_of_transaction: false,
        table: table.to_string(),
        target: IndexTargetSchema::Columns(vec![IndexColumnSchema {
            name: fts.column.to_string(),
            nulls: IndexNullsOrderSchema::Default,
            opclass: None,
            order: IndexOrderSchema::Asc,
        }]),
    }
}

fn fts_index_name(table: &str, column: &str) -> String {
    let full = format!("{table}_{column}_gin");
    if full.len() <= 63 {
        return full;
    }

    use std::hash::{BuildHasher, BuildHasherDefault, Hasher};
    let mut h =
        BuildHasherDefault::<std::collections::hash_map::DefaultHasher>::default().build_hasher();
    h.write(full.as_bytes());
    let digest = format!("{:08x}", h.finish() as u32);
    let stem: String = full.as_bytes()[..54].iter().map(|b| *b as char).collect();
    format!("{stem}_{digest}")
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
        IndexTarget, IndexType, ModelDescriptor, PkType, RangeSubtypeKind, field_descriptor,
        model_descriptor,
    };
    use crate::relation::registry::{
        RelationKind as RegistryRelationKind, RelationRegistryError, ReverseRelationMarker,
        validate_relation_accessor_collisions,
    };

    fn synth_collision_marker(
        kind: RegistryRelationKind,
        source: &'static str,
        name: &'static str,
        target: &'static str,
        via: &'static str,
    ) -> ReverseRelationMarker {
        crate::relation::registry::__macro_support::__make_reverse_relation_marker(
            kind, source, name, target, via,
        )
    }

    fn synth_model(table: &'static str, type_name: &'static str) -> ModelDescriptor {
        ModelDescriptor {
            ..model_descriptor(type_name, table, PkType::HeerIdDesc, &[])
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

    /// Phase 8β T3.5 — proxy descriptors are skipped from DDL emission
    /// (schema-passthrough). Two proxies of the same parent register
    /// alongside the parent without surfacing a duplicate-table-in-bucket
    /// collision; the parent's projection is the only one that reaches
    /// the bucket's `models` map.
    #[test]
    fn proxy_descriptors_skipped_from_projection() {
        // Parent + two proxies of it — all share `vehicles` as the table
        // name. Without the proxy_for skip, the second `synth_model`
        // call in the same bucket would trigger DuplicateTableInBucket.
        let parent = synth_model("vehicles", "Vehicle");
        let active = ModelDescriptor {
            proxy_for: Some("Vehicle"),
            default_filter_sql: Some("active = TRUE"),
            ..synth_model("vehicles", "ActiveVehicle")
        };
        let archived = ModelDescriptor {
            proxy_for: Some("Vehicle"),
            default_filter_sql: Some("archived = TRUE"),
            ..synth_model("vehicles", "ArchivedVehicle")
        };
        let buckets = project_from_iters(
            [&parent, &active, &archived],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("proxies coexist with parent without DDL collisions");
        let global = buckets.get(&empty_global()).expect("global");
        // Exactly one table — the parent's. Both proxies are skipped.
        assert_eq!(
            global.models.len(),
            1,
            "expected parent-only projection, got {} entries",
            global.models.len(),
        );
        assert!(global.models.contains_key("vehicles"));
    }

    /// Phase 8β T3.5 — proxy descriptors registering before the parent
    /// in inventory iteration order (which is non-deterministic per
    /// `inventory` semantics) still resolve cleanly: the parent's
    /// projection wins regardless of the order proxies appear in the
    /// input slice.
    #[test]
    fn proxy_skip_independent_of_iteration_order() {
        let parent = synth_model("vehicles", "Vehicle");
        let proxy = ModelDescriptor {
            proxy_for: Some("Vehicle"),
            default_filter_sql: Some("active = TRUE"),
            ..synth_model("vehicles", "ActiveVehicle")
        };
        // Proxy first, parent second — opposite of the previous test's
        // ordering. Result must be identical.
        let buckets = project_from_iters(
            [&proxy, &parent],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let global = buckets.get(&empty_global()).expect("global");
        assert_eq!(global.models.len(), 1);
        assert!(global.models.contains_key("vehicles"));
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
        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            indexed: true,
            relation_kind: Some(RelationKind::ForeignKey),
            on_delete: Some(OnDelete::Restrict),
            target_type_name: Some("Owner"),
            ..field_descriptor("owner_id", FieldSqlType::BigInt, false)
        }];
        let owner = synth_model("owners", "Owner");
        let vehicle = ModelDescriptor {
            fields: FIELDS,
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

    /// Helper for the FK-cascade round-trip test — `'static`
    /// FieldDescriptor slices must come from a real `static` slot
    /// because `ModelDescriptor.fields: &'static [FieldDescriptor]`.
    const fn fk_field_descriptor(on_delete: OnDelete) -> FieldDescriptor {
        FieldDescriptor {
            indexed: true,
            relation_kind: Some(RelationKind::ForeignKey),
            on_delete: Some(on_delete),
            target_type_name: Some("Owner"),
            ..field_descriptor("owner_id", FieldSqlType::BigInt, false)
        }
    }

    #[test]
    fn fk_cascade_round_trips_through_foreign_key_schema() {
        // Codex T3 review B-3: the column's declared `OnDelete` must
        // populate `ForeignKeySchema.on_delete` so the SQL emitter
        // can render the right `ON DELETE ...` clause without
        // silently coercing to RESTRICT.
        const RESTRICT: &[FieldDescriptor] = &[fk_field_descriptor(OnDelete::Restrict)];
        const CASCADE: &[FieldDescriptor] = &[fk_field_descriptor(OnDelete::Cascade)];
        const SET_NULL: &[FieldDescriptor] = &[fk_field_descriptor(OnDelete::SetNull)];
        const SET_DEFAULT: &[FieldDescriptor] = &[fk_field_descriptor(OnDelete::SetDefault)];
        const DO_NOTHING: &[FieldDescriptor] = &[fk_field_descriptor(OnDelete::DoNothing)];
        const PROTECT: &[FieldDescriptor] = &[fk_field_descriptor(OnDelete::Protect)];

        let owner = synth_model("owners", "Owner");
        for (slice, expected_label, expected) in [
            (RESTRICT, "Restrict", OnDeleteSchema::Restrict),
            (CASCADE, "Cascade", OnDeleteSchema::Cascade),
            (SET_NULL, "SetNull", OnDeleteSchema::SetNull),
            (SET_DEFAULT, "SetDefault", OnDeleteSchema::SetDefault),
            (
                DO_NOTHING,
                "DoNothing -> NoAction",
                OnDeleteSchema::NoAction,
            ),
            // Protect maps to Restrict (per `project_on_delete`).
            (PROTECT, "Protect -> Restrict", OnDeleteSchema::Restrict),
        ] {
            let vehicle = ModelDescriptor {
                fields: slice,
                ..synth_model("vehicles", "Vehicle")
            };
            let buckets = project_from_iters(
                [&owner, &vehicle],
                [],
                [],
                "2026-04-25T00:00:00Z".to_string(),
            )
            .expect("ok");
            let owner_id = &buckets[&empty_global()].models["vehicles"].columns[0];
            let fk = owner_id.foreign_key.as_ref().expect("fk");
            assert_eq!(
                fk.on_delete, expected,
                "{expected_label} must project to {expected:?}; got {:?}",
                fk.on_delete
            );
            // Column's `on_delete` field mirrors the FK's.
            assert_eq!(owner_id.on_delete, Some(expected));
        }
    }

    #[test]
    fn fk_deferrability_round_trips_through_foreign_key_schema() {
        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            indexed: true,
            relation_kind: Some(RelationKind::ForeignKey),
            on_delete: Some(OnDelete::Restrict),
            target_type_name: Some("Owner"),
            ..field_descriptor("owner_id", FieldSqlType::BigInt, false)
        }];
        let owner = synth_model("owners", "Owner");
        let vehicle = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("vehicles", "Vehicle")
        };
        static DEFERRABILITY: &[DeferrabilitySpec] = &[DeferrabilitySpec {
            model_type_name: "Vehicle",
            field_name: "owner_id",
            deferrable: true,
            initially_deferred: true,
        }];
        let buckets = project_from_iters_with_deferrability(
            [&owner, &vehicle],
            [],
            [],
            DEFERRABILITY,
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let fk = buckets[&empty_global()].models["vehicles"].columns[0]
            .foreign_key
            .as_ref()
            .expect("fk");
        assert!(fk.deferrable);
        assert!(fk.initially_deferred);
    }

    #[test]
    fn cross_bucket_fk_resolves_via_global_type_lookup() {
        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            indexed: true,
            relation_kind: Some(RelationKind::ForeignKey),
            on_delete: Some(OnDelete::Restrict),
            target_type_name: Some("User"),
            ..field_descriptor("user_id", FieldSqlType::BigInt, false)
        }];
        let billing = synth_app("billing", "main");
        let users = synth_app("users", "main");
        let user = ModelDescriptor {
            app: Some("users"),
            ..synth_model("users", "User")
        };
        let invoice = ModelDescriptor {
            app: Some("billing"),
            fields: FIELDS,
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
    fn pk_default_sql_uses_canonical_heeranjid_functions() {
        assert_eq!(
            pk_default_sql(&PkType::HeerId).as_deref(),
            Some("heerid_next()")
        );
        assert_eq!(
            pk_default_sql(&PkType::HeerIdDesc).as_deref(),
            Some("heerid_next_desc()")
        );
        assert_eq!(
            pk_default_sql(&PkType::RanjId).as_deref(),
            Some("ranjid_next()")
        );
        assert_eq!(
            pk_default_sql(&PkType::RanjIdDesc).as_deref(),
            Some("ranjid_next_desc()")
        );
    }

    #[test]
    fn pk_default_sql_is_heerid_next_desc_for_heer_id_desc_projection() {
        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            ..field_descriptor("id", FieldSqlType::BigInt, false)
        }];
        let m = ModelDescriptor {
            fields: FIELDS,
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
        assert_eq!(id_col.default_sql.as_deref(), Some("heerid_next_desc()"));
    }

    #[test]
    fn ddl_metadata_projects_from_model_and_field_descriptors() {
        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                comment: Some("Stable adopter-facing identifier"),
                ..field_descriptor("name", FieldSqlType::Text, false)
            },
            FieldDescriptor {
                ..field_descriptor("weight_kg", FieldSqlType::DoublePrecision, true)
            },
        ];
        let m = ModelDescriptor {
            fields: FIELDS,
            table_comment: Some("Operational metadata table"),
            storage_params: Some("fillfactor=70, autovacuum_enabled=false"),
            tablespace: Some("fastspace"),
            ..synth_model("widgets", "Widget")
        };

        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");

        let table = &buckets[&empty_global()].models["widgets"];
        assert_eq!(
            table.table_comment.as_deref(),
            Some("Operational metadata table")
        );
        assert_eq!(
            table.storage_params.as_deref(),
            Some("fillfactor=70, autovacuum_enabled=false")
        );
        assert_eq!(table.tablespace.as_deref(), Some("fastspace"));
        assert_eq!(
            table.columns[0].comment.as_deref(),
            Some("Stable adopter-facing identifier")
        );
        assert_eq!(table.columns[1].comment, None);
    }

    #[test]
    fn fts_projection_synthesizes_generated_column_and_gin_index() {
        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            },
            FieldDescriptor {
                ..field_descriptor("title", FieldSqlType::Text, false)
            },
            FieldDescriptor {
                ..field_descriptor("body", FieldSqlType::Text, false)
            },
        ];
        let m = ModelDescriptor {
            fields: FIELDS,
            fts: Some(FtsDescriptor {
                column: "search",
                source: "title, body",
                dictionary: "english",
            }),
            ..synth_model("book", "Book")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");

        let global = &buckets[&empty_global()];
        let table = &global.models["book"];
        let search = table
            .columns
            .iter()
            .find(|column| column.name == "search")
            .expect("generated search column");
        assert_eq!(search.sql_type, "TSVECTOR");
        assert_eq!(
            search.generated.as_ref().map(|g| g.expression.as_str()),
            Some("to_tsvector('english', \"title\" || ' ' || \"body\")")
        );
        assert!(search.generated.as_ref().is_some_and(|g| g.stored));

        assert_eq!(global.indexes.len(), 1);
        let index = &global.indexes[0];
        assert_eq!(index.name, "book_search_gin");
        assert_eq!(index.index_type, IndexTypeSchema::Gin);
        assert_eq!(
            index.target,
            IndexTargetSchema::Columns(vec![IndexColumnSchema {
                name: "search".to_string(),
                nulls: IndexNullsOrderSchema::Default,
                opclass: None,
                order: IndexOrderSchema::Asc,
            }])
        );
    }

    #[test]
    fn outbox_projection_synthesizes_sibling_table_for_heerid_pk() {
        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            ..field_descriptor("id", FieldSqlType::BigInt, false)
        }];
        let m = ModelDescriptor {
            pk_type: PkType::HeerId,
            fields: FIELDS,
            has_outbox: true,
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
        let models = &global.models;
        let names: Vec<&str> = models.keys().map(String::as_str).collect();
        assert_eq!(names, vec!["widgets", "widgets_outbox"]);

        let outbox = &models["widgets_outbox"];
        assert_eq!(outbox.table, "widgets_outbox");
        assert_eq!(outbox.columns[0].name, "id");
        assert_eq!(outbox.columns[0].sql_type, "BIGINT");
        assert_eq!(
            outbox.columns[0].default_sql.as_deref(),
            Some("heerid_next()")
        );
        assert_eq!(outbox.columns[1].name, "row_id");
        assert_eq!(outbox.columns[1].sql_type, "BIGINT");
        assert_eq!(outbox.columns[2].name, "action");
        assert_eq!(outbox.columns[2].sql_type, "TEXT");
        assert_eq!(
            outbox.columns[2].check.as_deref(),
            Some("action IN ('create', 'save', 'delete')"),
        );
        assert_eq!(outbox.columns[3].name, "payload");
        assert_eq!(outbox.columns[3].sql_type, "JSONB");
        assert_eq!(outbox.columns[4].name, "created_at");
        assert_eq!(outbox.columns[4].sql_type, "TIMESTAMPTZ");
        assert_eq!(outbox.columns[4].default_sql.as_deref(), Some("now()"));
        assert_eq!(outbox.columns[5].name, "state");
        assert_eq!(outbox.columns[5].sql_type, "TEXT");
        assert_eq!(outbox.columns[5].default_sql.as_deref(), Some("'pending'"));
        assert_eq!(
            outbox.columns[5].check.as_deref(),
            Some("state IN ('pending', 'processing', 'published', 'failed')"),
        );
        assert_eq!(outbox.columns[6].name, "leased_until");
        assert!(outbox.columns[6].nullable);
        assert_eq!(outbox.columns[7].name, "retry_count");
        assert_eq!(outbox.columns[7].default_sql.as_deref(), Some("0"));
        assert_eq!(outbox.columns[8].name, "failed_reason");
        assert!(outbox.columns[8].nullable);

        assert_eq!(global.indexes.len(), 1);
        let idx = &global.indexes[0];
        assert_eq!(idx.name, "widgets_outbox_pending_idx");
        assert_eq!(idx.table, "widgets_outbox");
        assert_eq!(idx.kind, IndexKindSchema::NonUnique);
        assert_eq!(idx.index_type, IndexTypeSchema::BTree);
        assert_eq!(idx.predicate.as_deref(), Some("state = 'pending'"));
        assert_eq!(
            idx.target,
            IndexTargetSchema::Columns(vec![
                IndexColumnSchema {
                    name: "state".to_string(),
                    nulls: IndexNullsOrderSchema::Default,
                    opclass: None,
                    order: IndexOrderSchema::Asc,
                },
                IndexColumnSchema {
                    name: "created_at".to_string(),
                    nulls: IndexNullsOrderSchema::Default,
                    opclass: None,
                    order: IndexOrderSchema::Asc,
                },
            ])
        );
    }

    #[test]
    fn outbox_projection_uses_uuid_row_id_for_ranjid_pk() {
        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            ..field_descriptor("id", FieldSqlType::Uuid, false)
        }];
        let m = ModelDescriptor {
            pk_type: PkType::RanjId,
            fields: FIELDS,
            has_outbox: true,
            ..synth_model("events", "Event")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let outbox = &buckets[&empty_global()].models["events_outbox"];
        assert_eq!(outbox.columns[1].name, "row_id");
        assert_eq!(outbox.columns[1].sql_type, "UUID");
    }

    #[test]
    fn outbox_projection_uses_custom_row_id_sql_type() {
        const CUSTOM_PK: crate::descriptor::CustomPrimaryKeyKind =
            crate::descriptor::CustomPrimaryKeyKind {
                type_name: "crate::ids::WidgetId",
                sql_type: "CITEXT",
                default_sql: "make_widget_id()",
            };
        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            ..field_descriptor("id", FieldSqlType::Citext, false)
        }];
        let m = ModelDescriptor {
            pk_type: PkType::Custom(CUSTOM_PK),
            fields: FIELDS,
            has_outbox: true,
            ..synth_model("custom_widgets", "CustomWidget")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let outbox = &buckets[&empty_global()].models["custom_widgets_outbox"];
        assert_eq!(outbox.columns[1].name, "row_id");
        assert_eq!(outbox.columns[1].sql_type, "CITEXT");
    }

    /// #86 — Serial PK must emit auto-increment IDENTITY clause via
    /// the dedicated `identity` field on `ColumnSchema` (not by
    /// inlining the clause into `sql_type`, which would route through
    /// `ColumnChange::ChangeType` and emit invalid `ALTER COLUMN ...
    /// TYPE INTEGER GENERATED BY DEFAULT AS IDENTITY` for snapshot
    /// upgrades — Codex round-1 BLOCK).
    ///
    /// The DDL emitter renders `INTEGER GENERATED BY DEFAULT AS
    /// IDENTITY NOT NULL` from `sql_type = "INTEGER"` plus
    /// `identity = Some(IdentityKindSchema::ByDefault)`. The differ
    /// detects identity additions and emits
    /// `ALTER COLUMN id ADD GENERATED BY DEFAULT AS IDENTITY` —
    /// the correct PG syntax for adding identity to existing tables.
    #[test]
    fn serial_pk_emits_identity_field_on_id_column() {
        use crate::migrate::schema::IdentityKindSchema;

        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            ..field_descriptor("id", FieldSqlType::Integer, false)
        }];
        let m = ModelDescriptor {
            pk_type: PkType::Serial,
            fields: FIELDS,
            ..synth_model("countries", "Country")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let id_col = &buckets[&empty_global()].models["countries"].columns[0];
        // sql_type stays plain "INTEGER" — the IDENTITY clause lives
        // on the dedicated `identity` field, not inlined.
        assert_eq!(
            id_col.sql_type, "INTEGER",
            "Serial PK id column sql_type must stay plain INTEGER (the IDENTITY clause lives on `identity`); got {}",
            id_col.sql_type
        );
        assert_eq!(
            id_col.identity,
            Some(IdentityKindSchema::ByDefault),
            "Serial PK id column must carry `identity = Some(ByDefault)`"
        );
        // default_sql stays None — IDENTITY is not a DEFAULT
        // expression. The DDL emitter handles them separately.
        assert_eq!(
            id_col.default_sql, None,
            "IDENTITY columns must not also carry a DEFAULT expression"
        );
    }

    /// #86 sibling — FK columns *referencing* a Serial PK must NOT
    /// inherit the IDENTITY clause. Sequence ownership lives on the
    /// PK column itself, not on its FK references.
    #[test]
    fn serial_pk_fk_references_stay_plain_integer_no_identity() {
        static FK_TO_COUNTRY: &[FieldDescriptor] = &[FieldDescriptor {
            indexed: true,
            relation_kind: Some(crate::descriptor::RelationKind::ForeignKey),
            on_delete: Some(crate::descriptor::OnDelete::Restrict),
            target_type_name: Some("Country"),
            ..field_descriptor("country_id", FieldSqlType::Integer, false)
        }];
        let country = ModelDescriptor {
            pk_type: PkType::Serial,
            ..synth_model("countries", "Country")
        };
        let herd = ModelDescriptor {
            fields: FK_TO_COUNTRY,
            ..synth_model("herds", "Herd")
        };
        let buckets = project_from_iters(
            [&country, &herd],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let country_id_col = &buckets[&empty_global()].models["herds"].columns[0];
        assert_eq!(
            country_id_col.sql_type, "INTEGER",
            "FK to Serial PK keeps plain INTEGER sql_type; got {}",
            country_id_col.sql_type
        );
        assert_eq!(
            country_id_col.identity, None,
            "FK to Serial PK must NOT carry an `identity` clause"
        );
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

    #[test]
    fn cross_database_fk_rejected_at_projection() {
        // Codex T2 review B-3: an FK from a model in one database to
        // a model in another database is structurally invalid
        // (Postgres FKs cannot span databases). Projection rejects
        // before producing a snapshot.
        let main_app = AppDescriptor {
            label: "billing",
            database: "main",
            renamed_from: None,
            tombstone: false,
        };
        let crud_app = AppDescriptor {
            label: "audit",
            database: "crud_log",
            renamed_from: None,
            tombstone: false,
        };
        let target = ModelDescriptor {
            app: Some("audit"),
            ..synth_model("audit_rows", "AuditRow")
        };
        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            indexed: true,
            relation_kind: Some(RelationKind::ForeignKey),
            on_delete: Some(OnDelete::Restrict),
            target_type_name: Some("AuditRow"),
            ..field_descriptor("audit_id", FieldSqlType::BigInt, false)
        }];
        let source = ModelDescriptor {
            app: Some("billing"),
            fields: FIELDS,
            ..synth_model("invoices", "Invoice")
        };
        let err = project_from_iters(
            [&target, &source],
            std::iter::empty::<&EnumDescriptor>(),
            [&main_app, &crud_app],
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect_err("must reject cross-DB FK");
        match err {
            ProjectionError::CrossDatabaseForeignKey {
                source_table,
                target_table,
                source_bucket,
                target_bucket,
                ..
            } => {
                assert_eq!(source_table, "invoices");
                assert_eq!(target_table, "audit_rows");
                assert_eq!(source_bucket.database, "main");
                assert_eq!(target_bucket.database, "crud_log");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// Phase 8β BLOCK-2 — FK targeting a proxy in another database
    /// must resolve THROUGH the proxy to its parent and trip
    /// CrossDatabaseForeignKey when the *parent* (not the proxy) lives
    /// in a different database from the source. Today's pre-fix code
    /// would compare the proxy's bucket against the source's bucket;
    /// with the fix, the parent's bucket is the canonical target.
    #[test]
    fn fk_to_proxy_resolves_through_to_parent_for_cross_db_check() {
        // Parent is in `crud_log/audit`. Proxy of the parent is also
        // declared with `app=audit` so proxy and parent share a bucket.
        // FK source (in `main/billing`) targets the proxy. The
        // resolution walker should land on the parent's bucket
        // (crud_log/audit), and the cross-DB check should fire.
        let main_app = AppDescriptor {
            label: "billing",
            database: "main",
            renamed_from: None,
            tombstone: false,
        };
        let crud_app = AppDescriptor {
            label: "audit",
            database: "crud_log",
            renamed_from: None,
            tombstone: false,
        };
        let parent = ModelDescriptor {
            app: Some("audit"),
            ..synth_model("audit_rows", "AuditRow")
        };
        let proxy = ModelDescriptor {
            app: Some("audit"),
            proxy_for: Some("AuditRow"),
            ..synth_model("audit_rows", "ActiveAuditRow")
        };
        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            indexed: true,
            relation_kind: Some(RelationKind::ForeignKey),
            on_delete: Some(OnDelete::Restrict),
            // Target the PROXY, not the parent.
            target_type_name: Some("ActiveAuditRow"),
            ..field_descriptor("audit_id", FieldSqlType::BigInt, false)
        }];
        let source = ModelDescriptor {
            app: Some("billing"),
            fields: FIELDS,
            ..synth_model("invoices", "Invoice")
        };
        let err = project_from_iters(
            [&parent, &proxy, &source],
            std::iter::empty::<&EnumDescriptor>(),
            [&main_app, &crud_app],
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect_err("must reject cross-DB FK even when the FK targets a proxy");
        match err {
            ProjectionError::CrossDatabaseForeignKey {
                source_table,
                target_table,
                source_bucket,
                target_bucket,
                ..
            } => {
                assert_eq!(source_table, "invoices");
                // Target table is the parent's table — proxies share it.
                assert_eq!(target_table, "audit_rows");
                assert_eq!(source_bucket.database, "main");
                assert_eq!(target_bucket.database, "crud_log");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// Phase 8β BLOCK-2 — FK to a proxy whose `proxy_for` parent is
    /// not registered in the inventory must surface
    /// `ProxyParentNotRegistered`. Without this gate, the FK would
    /// silently emit `REFERENCES <parent_table>(id)` against a table
    /// no projection step has added.
    #[test]
    fn fk_to_proxy_with_unregistered_parent_rejected() {
        let proxy = ModelDescriptor {
            proxy_for: Some("MissingParent"),
            ..synth_model("vehicles", "ActiveVehicle")
        };
        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            indexed: true,
            relation_kind: Some(RelationKind::ForeignKey),
            on_delete: Some(OnDelete::Restrict),
            target_type_name: Some("ActiveVehicle"),
            ..field_descriptor("vehicle_id", FieldSqlType::BigInt, false)
        }];
        let source = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("invoices", "Invoice")
        };
        let err = project_from_iters(
            [&proxy, &source],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect_err("must reject FK whose proxy parent is unregistered");
        match err {
            ProjectionError::ProxyParentNotRegistered {
                source_table,
                proxy_type,
                parent_type,
                ..
            } => {
                assert_eq!(source_table, "invoices");
                assert_eq!(proxy_type, "ActiveVehicle");
                assert_eq!(parent_type, "MissingParent");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// Phase 8β BLOCK-2 — same-database FK to a proxy whose parent
    /// also sits in the same database resolves cleanly. The walker
    /// crosses the proxy and lands on the parent; the cross-DB check
    /// short-circuits because both sides share the database.
    #[test]
    fn fk_to_proxy_same_database_passes() {
        let parent = synth_model("vehicles", "Vehicle");
        let proxy = ModelDescriptor {
            proxy_for: Some("Vehicle"),
            ..synth_model("vehicles", "ActiveVehicle")
        };
        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            indexed: true,
            relation_kind: Some(RelationKind::ForeignKey),
            on_delete: Some(OnDelete::Restrict),
            target_type_name: Some("ActiveVehicle"),
            ..field_descriptor("vehicle_id", FieldSqlType::BigInt, false)
        }];
        let source = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("invoices", "Invoice")
        };
        let buckets = project_from_iters(
            [&parent, &proxy, &source],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("same-DB proxy FK passes validation");
        // Source landed in the global bucket; FK target resolved to
        // the parent's table.
        let global = buckets.get(&empty_global()).expect("global");
        let invoice = global
            .models
            .get("invoices")
            .expect("invoices model present");
        let fk_col = invoice
            .columns
            .iter()
            .find(|c| c.name == "vehicle_id")
            .expect("FK column emitted");
        let fk = fk_col.foreign_key.as_ref().expect("FK metadata present");
        assert_eq!(fk.ref_table, "vehicles");
    }

    // ── Codex T10 round-1 regression tests ──────────────────────────
    //
    // The three substrate fixes shipped with T10 (framework-col
    // defaults, FK column SQL type substitution, `Jsonb<T>` recognition
    // in the macros' `rust_type_to_sql`) only had indirect coverage
    // through the live `phase7_t10_sync_models_live.rs` integration
    // suite. The Codex round-1 review flagged this gap (Concern 1
    // PARTIAL) — the rules now have direct unit tests so a regression
    // surfaces here without needing a Postgres-backed run.

    /// Phase 7 T10 — framework-injected timestamp columns must carry
    /// `DEFAULT now()` so a typed `Model::create` round-trip without an
    /// explicit value picks up server time. Without this the first
    /// INSERT into a freshly-`sync_models`'d table fails with `null
    /// value in column "created_at"`.
    #[test]
    fn framework_timestamp_cols_get_now_default() {
        const FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("created_at", FieldSqlType::Timestamptz, false)
            },
            FieldDescriptor {
                ..field_descriptor("updated_at", FieldSqlType::Timestamptz, false)
            },
        ];
        let m = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("widgets", "Widget")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let columns = &buckets[&empty_global()].models["widgets"].columns;
        let created = columns.iter().find(|c| c.name == "created_at").unwrap();
        let updated = columns.iter().find(|c| c.name == "updated_at").unwrap();
        assert_eq!(
            created.default_sql.as_deref(),
            Some("now()"),
            "created_at must get DEFAULT now()"
        );
        assert_eq!(
            updated.default_sql.as_deref(),
            Some("now()"),
            "updated_at must get DEFAULT now()"
        );
    }

    /// Phase 7 T10 — only the two framework-injected timestamp columns
    /// (`created_at`, `updated_at`) receive the `now()` default. A
    /// user-declared Timestamptz column with a different name passes
    /// through with `default_sql = None` so the descriptor layer's
    /// "no per-field defaults today" contract still holds.
    #[test]
    fn non_framework_timestamptz_col_has_no_default() {
        const FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            ..field_descriptor("shipped_at", FieldSqlType::Timestamptz, true)
        }];
        let m = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("widgets", "Widget")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let shipped = &buckets[&empty_global()].models["widgets"].columns[0];
        assert_eq!(
            shipped.default_sql, None,
            "user-declared Timestamptz must NOT get the framework default"
        );
    }

    /// Phase 7 T10 — FK column's SQL type is substituted to match the
    /// target model's PK SQL type. The descriptor layer cannot do this
    /// lookup (each model's descriptor is emitted in isolation; the FK
    /// target may live in a separate crate), so the projection is the
    /// canonical resolution site. Covers the four built-in PK shapes
    /// plus a Custom PK with a verbatim sql_type.
    #[test]
    fn fk_column_sql_type_substituted_from_target_pk() {
        const FK_TO_OWNER: &[FieldDescriptor] = &[FieldDescriptor {
            // Macro emits a placeholder type — projection must replace
            // it with the target's PK SQL type. Use BigInt as the
            // placeholder so a regression where substitution silently
            // skips would leave `BIGINT` and the RanjId/Serial assertions
            // would fail.
            indexed: true,
            relation_kind: Some(RelationKind::ForeignKey),
            on_delete: Some(OnDelete::Restrict),
            target_type_name: Some("Owner"),
            ..field_descriptor("owner_id", FieldSqlType::BigInt, false)
        }];

        // Each row: target PK type, expected substituted SQL on the FK
        // column. (HeerId / HeerIdDesc → BIGINT; RanjId / RanjIdDesc →
        // UUID; Serial → INTEGER; Custom → that type's `sql_type`
        // verbatim — `CITEXT` is a Postgres extension type chosen here
        // to make a "no substitution" regression jump out.)
        const CUSTOM_PK: crate::descriptor::CustomPrimaryKeyKind =
            crate::descriptor::CustomPrimaryKeyKind {
                type_name: "crate::ids::OwnerId",
                sql_type: "CITEXT",
                default_sql: "",
            };
        let cases: [(PkType, &str); 6] = [
            (PkType::HeerId, "BIGINT"),
            (PkType::HeerIdDesc, "BIGINT"),
            (PkType::RanjId, "UUID"),
            (PkType::RanjIdDesc, "UUID"),
            (PkType::Serial, "INTEGER"),
            (PkType::Custom(CUSTOM_PK), "CITEXT"),
        ];

        for (pk, expected_sql) in cases {
            let owner = ModelDescriptor {
                pk_type: pk,
                ..synth_model("owners", "Owner")
            };
            let vehicle = ModelDescriptor {
                fields: FK_TO_OWNER,
                ..synth_model("vehicles", "Vehicle")
            };
            let buckets = project_from_iters(
                [&owner, &vehicle],
                std::iter::empty::<&EnumDescriptor>(),
                std::iter::empty::<&AppDescriptor>(),
                "2026-04-25T00:00:00Z".to_string(),
            )
            .expect("ok");
            let owner_id_col = &buckets[&empty_global()].models["vehicles"].columns[0];
            assert_eq!(
                owner_id_col.sql_type, expected_sql,
                "FK to Owner with PK {pk:?} must project owner_id.sql_type = {expected_sql}; got {}",
                owner_id_col.sql_type
            );
        }
    }

    /// Phase 7 T10 — non-FK columns pass through `f.sql_type` verbatim.
    /// Ensures the substitution only fires when `relation_kind` is
    /// `Some(_)` AND `target_type_name` resolves in the type map.
    ///
    /// Codex T10 round-2 sharpened scenario: a non-FK column on the
    /// SAME model where another field is a FK to a model with a DIFFERENT
    /// PK SQL type. If a regression in the substitution rule walked
    /// every column instead of only relation columns, the non-FK column
    /// here (`SmallInt` placeholder type) would be incorrectly
    /// rewritten to the target's PK type. Asserting the original
    /// `SMALLINT` survives proves the per-field guard at projection.rs
    /// where `relation_kind.is_some()` gates the substitution.
    #[test]
    fn non_fk_column_sql_type_passes_through_verbatim() {
        const FIELDS: &[FieldDescriptor] = &[
            // Real FK column — substitution applies; ends up as UUID
            // because Owner has a RanjId PK below.
            FieldDescriptor {
                indexed: true,
                relation_kind: Some(RelationKind::ForeignKey),
                on_delete: Some(OnDelete::Restrict),
                target_type_name: Some("Owner"),
                ..field_descriptor("owner_id", FieldSqlType::BigInt, false)
            },
            // Non-FK SmallInt — must NOT be rewritten to UUID (or to
            // anything else) just because it lives on a model with FK
            // fields. The substitution rule is per-field, gated on
            // `relation_kind.is_some()`.
            FieldDescriptor {
                ..field_descriptor("sort_order", FieldSqlType::SmallInt, false)
            },
            // Non-FK Text — also must pass through verbatim.
            FieldDescriptor {
                ..field_descriptor("name", FieldSqlType::Text, false)
            },
        ];
        let owner = ModelDescriptor {
            // RanjId PK so the substitution target type (UUID) is
            // visibly different from the non-FK column's declared
            // SmallInt — a regression that swept the substitution
            // across non-relation columns would jump out.
            pk_type: PkType::RanjId,
            ..synth_model("owners", "Owner")
        };
        let widget = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("widgets", "Widget")
        };
        let buckets = project_from_iters(
            [&owner, &widget],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let cols = &buckets[&empty_global()].models["widgets"].columns;
        let owner_id = cols.iter().find(|c| c.name == "owner_id").unwrap();
        let sort_order = cols.iter().find(|c| c.name == "sort_order").unwrap();
        let name = cols.iter().find(|c| c.name == "name").unwrap();
        // FK column: substituted to UUID (Owner's RanjId PK SQL type).
        assert_eq!(
            owner_id.sql_type, "UUID",
            "FK to Owner (RanjId PK) must substitute to UUID; got {}",
            owner_id.sql_type
        );
        // Non-FK columns: passed through verbatim, NOT rewritten to
        // UUID even though they live alongside an FK column whose
        // target has a UUID PK.
        assert_eq!(
            sort_order.sql_type, "SMALLINT",
            "non-FK SmallInt column must pass through unchanged; got {}",
            sort_order.sql_type
        );
        assert_eq!(
            name.sql_type, "TEXT",
            "non-FK Text column must pass through unchanged; got {}",
            name.sql_type
        );
    }

    // ── GH #158 — projection-time relation-registry gate ──────────────────
    //
    // The full integration (live `inventory::iter::<ReverseRelationMarker>`
    // walk → `project_from_inventory()` failure) is intentionally NOT
    // pinned with a globally-submitted colliding marker: such a submission
    // would persist for every other test in the lib's test binary and
    // permanently lock `project_from_inventory()` into the failing branch.
    // Instead we exercise the wrapping path through
    // `project_from_inventory_with_relation_validator`, which the
    // production entry point delegates to. Synthetic registry errors are
    // built via the public `validate_relation_accessor_collisions` so the
    // shape matches what the live walker would emit.

    #[test]
    fn project_from_inventory_wraps_relation_collision_into_projection_error() {
        // Build a synthetic `RelationRegistryError` using the public API
        // so the wrapper sees the exact shape `validate_global_relation_accessor_registry`
        // would emit against an offending live registry.
        let markers = [
            synth_collision_marker(
                RegistryRelationKind::FK,
                "Owner",
                "cars",
                "Vehicle",
                "owner_id",
            ),
            synth_collision_marker(
                RegistryRelationKind::M2M,
                "Owner",
                "cars",
                "Garage",
                "owner_id",
            ),
        ];
        let registry_err: RelationRegistryError =
            validate_relation_accessor_collisions(markers.iter())
                .expect_err("synthetic FK + M2M markers must collide");

        let result = project_from_inventory_with_relation_validator(|| Err(registry_err));
        let err = result.expect_err("validator failure must short-circuit projection");

        // Variant assertion — the gate must surface as the dedicated
        // `RelationAccessorCollisions` arm so callers can match on it
        // (e.g. CLI `compose_cmd` may eventually want a tailored exit
        // code or ANSI hint for this case).
        assert!(
            matches!(err, ProjectionError::RelationAccessorCollisions(_)),
            "expected RelationAccessorCollisions, got {err:?}"
        );

        // Diagnostic anchors — the message must point at the relation
        // registry metadata (source / accessor name / both kinds) and
        // carry the GH issue number so a future operator can grep for
        // it without re-discovering the design rationale.
        let msg = err.to_string();
        assert!(
            msg.contains("relation-accessor collisions detected before projection"),
            "missing projection-side anchor: {msg}"
        );
        assert!(msg.contains("GH #158"), "missing issue number: {msg}");
        assert!(msg.contains("Owner"), "missing source: {msg}");
        assert!(msg.contains("cars"), "missing accessor name: {msg}");
        assert!(msg.contains("FK"), "missing FK kind: {msg}");
        assert!(msg.contains("M2M"), "missing M2M kind: {msg}");
    }

    #[test]
    fn project_from_inventory_with_clean_validator_does_not_short_circuit() {
        // Inject a `Ok(())` validator. The downstream `project_from_iters`
        // call may still return `Ok` or `Err` depending on what the live
        // test-binary inventory holds (e.g. fixtures from other tests),
        // but the failure mode this test pins is "validator returned Ok →
        // we did NOT short-circuit with `RelationAccessorCollisions`".
        let result = project_from_inventory_with_relation_validator(|| Ok(()));
        if let Err(ref e) = result {
            assert!(
                !matches!(e, ProjectionError::RelationAccessorCollisions(_)),
                "validator returned Ok(()) but projection still produced \
                 RelationAccessorCollisions: {e}"
            );
        }
    }

    #[test]
    fn projection_error_relation_accessor_collisions_display_is_actionable() {
        // The Display impl is the operator-facing diagnostic surface.
        // Pin every load-bearing substring so a refactor cannot silently
        // drop the source / accessor / kinds / GH-issue anchor.
        let markers = [
            synth_collision_marker(
                RegistryRelationKind::FK,
                "Account",
                "subscriptions",
                "Subscription",
                "account_id",
            ),
            synth_collision_marker(
                RegistryRelationKind::M2M,
                "Account",
                "subscriptions",
                "Plan",
                "account_id",
            ),
        ];
        let registry_err = validate_relation_accessor_collisions(markers.iter()).unwrap_err();
        let projection_err = ProjectionError::RelationAccessorCollisions(registry_err);
        let msg = projection_err.to_string();
        assert!(msg.contains("Account"), "missing source: {msg}");
        assert!(msg.contains("subscriptions"), "missing accessor: {msg}");
        assert!(msg.contains("Subscription"), "missing FK target: {msg}");
        assert!(msg.contains("Plan"), "missing M2M target: {msg}");
        assert!(msg.contains("GH #158"), "missing issue anchor: {msg}");
    }

    // ── djogi#187 — temporal year-bounds CHECK projection ──────────────────
    //
    // `field_type_check` projects a year ±9999 CHECK on `Date` and
    // `Timestamptz` columns to match `time::Date` and
    // `time::OffsetDateTime` representable range. These arms ship
    // active because `FieldSqlType::Date` and
    // `FieldSqlType::Timestamptz` each have a single Rust source type
    // that lowers to them, so dispatching on `FieldSqlType` alone is
    // unambiguous.

    #[test]
    fn field_type_check_for_date_emits_year_upper_bound() {
        let expr = field_type_check(&FieldSqlType::Date, None, "birthday")
            .expect("DATE field must carry the time::Date year-bound CHECK");
        // One-sided upper bound by design — see the doc comment on
        // `field_type_check`. Postgres's date input parser rejects
        // every value `time::Date` cannot represent on the lower end
        // (4713 BC is Postgres's MIN), so the lower-bound CHECK is
        // redundant.
        assert!(
            expr.contains("\"birthday\" <= DATE '9999-12-31'"),
            "DATE CHECK upper bound: {expr}"
        );
        assert!(
            !expr.contains(">= DATE"),
            "DATE CHECK is one-sided upper bound (no lower-bound clause): {expr}"
        );
    }

    #[test]
    fn field_type_check_for_timestamptz_emits_year_upper_bound() {
        let expr = field_type_check(&FieldSqlType::Timestamptz, None, "occurred_at")
            .expect("TIMESTAMPTZ field must carry the OffsetDateTime year-bound CHECK");
        // The literal must use the TIMESTAMPTZ type keyword with an explicit +00
        // UTC offset so the comparison is timezone-invariant. Using TIMESTAMP
        // (without TZ) against a TIMESTAMPTZ column would make Postgres interpret
        // the literal in the session timezone, shifting the effective upper bound.
        assert!(
            expr.contains("\"occurred_at\" <= TIMESTAMPTZ '9999-12-31 23:59:59.999999+00'"),
            "TIMESTAMPTZ CHECK must use explicit UTC offset +00, not bare TIMESTAMP: {expr}"
        );
        assert!(
            !expr.contains(">= TIMESTAMPTZ"),
            "TIMESTAMPTZ CHECK is one-sided upper bound (no lower-bound clause): {expr}"
        );
        // Must not use the plain TIMESTAMP form (which is timezone-sensitive).
        assert!(
            !expr.contains("<= TIMESTAMP '"),
            "TIMESTAMPTZ CHECK must not use plain TIMESTAMP literal (timezone-sensitive): {expr}"
        );
    }

    #[test]
    fn field_type_check_for_timestamptz_is_utc_explicit() {
        // The +00 suffix is mandatory — a non-UTC session timezone must not
        // change the CHECK semantics. This test documents the invariant that the
        // projected SQL is always the same string regardless of caller context.
        let expr1 = field_type_check(&FieldSqlType::Timestamptz, None, "ts")
            .expect("TIMESTAMPTZ must produce a CHECK");
        let expr2 = field_type_check(&FieldSqlType::Timestamptz, None, "ts")
            .expect("repeated call must produce the same CHECK");
        assert_eq!(expr1, expr2, "CHECK expression must be deterministic");
        assert!(
            expr1.contains("+00'"),
            "CHECK literal must carry explicit +00 UTC offset: {expr1}"
        );
    }

    #[test]
    fn field_type_check_quotes_reserved_word_columns() {
        // `order` is a Postgres reserved word — the CHECK expression
        // must round-trip the column name through `quote_ident` so
        // the parser accepts the column reference. Use a `Date` field
        // since temporal arms are active today.
        let expr = field_type_check(&FieldSqlType::Date, None, "order")
            .expect("DATE field must carry CHECK regardless of column name");
        assert!(
            expr.contains("\"order\""),
            "CHECK expression must quote reserved-word column names: {expr}"
        );
    }

    #[test]
    fn field_type_check_returns_none_for_identity_widths() {
        // Identity-mapped Rust types lower to a Postgres column type
        // that already enforces their representable range; no
        // Rust-derived CHECK applies. The integer SQL types without a
        // `rust_source_type` discriminator (i16, i32, i64) also return
        // `None` — the discriminator gate ensures only the narrow/unsigned
        // Rust types (i8/u8/u16/u32/u64) get a CHECK.
        for ty in [
            FieldSqlType::Text,
            FieldSqlType::Boolean,
            FieldSqlType::Real,
            FieldSqlType::DoublePrecision,
            FieldSqlType::Numeric,
            FieldSqlType::Uuid,
            FieldSqlType::Jsonb,
            FieldSqlType::TextArray,
            FieldSqlType::SmallIntArray,
            FieldSqlType::IntegerArray,
            FieldSqlType::BigIntArray,
            FieldSqlType::RealArray,
            FieldSqlType::DoublePrecisionArray,
            FieldSqlType::BoolArray,
            FieldSqlType::UuidArray,
            FieldSqlType::Citext,
        ] {
            assert!(
                field_type_check(&ty, None, "col").is_none(),
                "non-widened SQL type {ty:?} must not carry a Rust-derived CHECK",
            );
        }
    }

    #[test]
    fn field_type_check_returns_none_for_direct_integer_widths_without_discriminator() {
        // `i16 → SmallInt`, `i32 → Integer`, `i64 → BigInt` columns have no
        // `rust_source_type` discriminator (`None`). The gate ensures they
        // never receive a narrow/unsigned range CHECK.
        //
        // A bare-NUMERIC column with `rust_source_type: None` is reached only
        // by user-defined scalar types (`DjogiSqlType::SQL_TYPE = "NUMERIC"`).
        // Those have no representable-range claim that the framework can
        // make on the adopter's behalf, so they keep `check: None`.
        // Adopter `Decimal` columns are NOT in this set — they carry
        // `Some(RustSourceType::Decimal)` and project the structural
        // CHECK via the Numeric arm of `field_type_check` (djogi#188).
        for ty in [
            FieldSqlType::SmallInt,
            FieldSqlType::Integer,
            FieldSqlType::BigInt,
            FieldSqlType::Numeric,
        ] {
            assert!(
                field_type_check(&ty, None, "col").is_none(),
                "direct-mapped integer/numeric SQL type {ty:?} with no rust_source_type \
                 must not carry a Rust-derived CHECK",
            );
        }
    }

    // ── djogi#190 — integer widening CHECK projection (now live) ──────────
    //
    // `field_type_check` now emits range CHECKs for the five narrow /
    // unsigned Rust types, gated on the `rust_source_type` discriminator.
    // Each test drives the helper directly and asserts the expression string.

    #[test]
    fn field_type_check_for_i8_smallint_emits_signed_byte_bounds() {
        let expr = field_type_check(
            &FieldSqlType::SmallInt,
            Some(RustSourceType::I8),
            "byte_col",
        )
        .expect("i8 → SmallInt must carry a range CHECK");
        assert!(
            expr.contains("\"byte_col\" >= -128 AND \"byte_col\" <= 127"),
            "i8 CHECK must cover -128..=127: {expr}"
        );
    }

    #[test]
    fn field_type_check_for_u8_smallint_emits_unsigned_byte_bounds() {
        let expr = field_type_check(&FieldSqlType::SmallInt, Some(RustSourceType::U8), "count")
            .expect("u8 → SmallInt must carry a range CHECK");
        assert!(
            expr.contains("\"count\" >= 0 AND \"count\" <= 255"),
            "u8 CHECK must cover 0..=255: {expr}"
        );
    }

    #[test]
    fn field_type_check_for_u16_integer_emits_unsigned_short_bounds() {
        let expr = field_type_check(&FieldSqlType::Integer, Some(RustSourceType::U16), "port")
            .expect("u16 → Integer must carry a range CHECK");
        assert!(
            expr.contains("\"port\" >= 0 AND \"port\" <= 65535"),
            "u16 CHECK must cover 0..=65535: {expr}"
        );
    }

    #[test]
    fn field_type_check_for_u32_bigint_emits_unsigned_int_bounds() {
        let expr = field_type_check(&FieldSqlType::BigInt, Some(RustSourceType::U32), "qty")
            .expect("u32 → BigInt must carry a range CHECK");
        assert!(
            expr.contains("\"qty\" >= 0 AND \"qty\" <= 4294967295"),
            "u32 CHECK must cover 0..=4294967295: {expr}"
        );
    }

    #[test]
    fn field_type_check_for_u64_numeric_emits_unsigned_long_bounds_with_integrality() {
        // u64 → bare NUMERIC: range bounds AND integrality check.
        // The integrality clause (col = trunc(col)) prevents fractional values
        // stored via raw SQL from bypassing the decode-side rejection.
        let expr = field_type_check(
            &FieldSqlType::Numeric,
            Some(RustSourceType::U64),
            "huge_count",
        )
        .expect("u64 → Numeric must carry a range+integrality CHECK");
        assert!(
            expr.contains("\"huge_count\" >= 0"),
            "u64 CHECK must have lower bound 0: {expr}"
        );
        assert!(
            expr.contains("\"huge_count\" <= 18446744073709551615"),
            "u64 CHECK must cover 0..=u64::MAX: {expr}"
        );
        assert!(
            expr.contains("\"huge_count\" = trunc(\"huge_count\")"),
            "u64 CHECK must reject fractional values via trunc: {expr}"
        );
    }

    #[test]
    fn field_type_check_source_type_mismatch_returns_none() {
        // A SmallInt column with a U16 discriminator (impossible in practice
        // since the macro always aligns the SQL type and source type, but the
        // function contract should handle it gracefully) returns `None` rather
        // than panicking.
        assert!(
            field_type_check(&FieldSqlType::SmallInt, Some(RustSourceType::U16), "col").is_none(),
            "U16 discriminator on SmallInt must return None (wrong carrier)"
        );
        assert!(
            field_type_check(&FieldSqlType::Integer, Some(RustSourceType::I8), "col").is_none(),
            "I8 discriminator on Integer must return None (wrong carrier)"
        );
    }

    // ── djogi#190 — integer source-type discriminator projection tests ────
    //
    // `project_column` passes `f.rust_source_type` to `field_type_check`.
    // Columns WITHOUT a `rust_source_type` discriminator (i.e. direct-mapped
    // `i16 → SmallInt`, `i32 → Integer`, `i64 → BigInt`) keep `check: None`
    // — the discriminator gate prevents spurious CHECKs on non-widened
    // columns. Columns WITH a discriminator (`i8/u8/u16/u32/u64`) receive
    // the corresponding range CHECK.
    //
    // The "guard" tests below assert that direct-mapped integer columns
    // (without `rust_source_type`) remain CHECK-free. The HeerId-backed `id`
    // column (`BigInt`, no discriminator) is the canonical case: if it ever
    // received a u32 CHECK it would reject every HeerId value above ~4.3B.

    #[test]
    fn project_column_no_check_for_non_fk_bigint_id_column_without_discriminator() {
        // Framework's `id: HeerId` column: BigInt, no rust_source_type.
        // Must never receive a range CHECK — HeerId values routinely exceed
        // u32::MAX (recency-biased IDs are at the TOP of the i64 range).
        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            ..field_descriptor("id", FieldSqlType::BigInt, false)
        }];
        let m = ModelDescriptor {
            fields: FIELDS,
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
        assert!(
            id_col.check.is_none(),
            "BigInt id column without rust_source_type must have check=None; got {:?}",
            id_col.check
        );
    }

    #[test]
    fn project_column_no_check_for_non_fk_bigint_amount_column_without_discriminator() {
        // `amount: i64` — BigInt, no rust_source_type. Must never receive a
        // range CHECK because i64 values legitimately span the full signed
        // 64-bit range, including values above u32::MAX and below zero.
        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            },
            FieldDescriptor {
                ..field_descriptor("amount", FieldSqlType::BigInt, false)
            },
        ];
        let m = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("ledgers", "Ledger")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let amount_col = &buckets[&empty_global()].models["ledgers"].columns[1];
        assert_eq!(amount_col.name, "amount");
        assert!(
            amount_col.check.is_none(),
            "BigInt amount column without rust_source_type must have check=None; got {:?}",
            amount_col.check
        );
    }

    #[test]
    fn project_column_no_check_for_non_fk_smallint_column_without_discriminator() {
        // `byte_count: i16` — SmallInt, no rust_source_type. Must never
        // receive an i8 CHECK (`>= -128 AND <= 127`) that would reject
        // i16 values in -32768..=-129 and 128..=32767.
        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            },
            FieldDescriptor {
                ..field_descriptor("byte_count", FieldSqlType::SmallInt, false)
            },
        ];
        let m = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("widgets", "Widget")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let byte_col = &buckets[&empty_global()].models["widgets"].columns[1];
        assert_eq!(byte_col.name, "byte_count");
        assert!(
            byte_col.check.is_none(),
            "SmallInt column without rust_source_type must have check=None; got {:?}",
            byte_col.check
        );
    }

    // Positive: project_column DOES emit CHECK for columns with a
    // rust_source_type discriminator.

    #[test]
    fn project_column_emits_check_for_u8_smallint() {
        use crate::descriptor::RustSourceType;
        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            },
            FieldDescriptor {
                rust_source_type: Some(RustSourceType::U8),
                ..field_descriptor("count", FieldSqlType::SmallInt, false)
            },
        ];
        let m = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("counters", "Counter")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-05-15T00:00:00Z".to_string(),
        )
        .expect("ok");
        let count_col = &buckets[&empty_global()].models["counters"].columns[1];
        assert_eq!(count_col.name, "count");
        let check = count_col
            .check
            .as_deref()
            .expect("u8 → SmallInt column with RustSourceType::U8 must have a range CHECK");
        assert!(
            check.contains(">= 0") && check.contains("<= 255"),
            "u8 CHECK must cover 0..=255: {check}"
        );
    }

    #[test]
    fn project_column_emits_check_for_u32_bigint() {
        use crate::descriptor::RustSourceType;
        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            },
            FieldDescriptor {
                rust_source_type: Some(RustSourceType::U32),
                ..field_descriptor("medium_count", FieldSqlType::BigInt, false)
            },
        ];
        let m = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("things", "Thing")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-05-15T00:00:00Z".to_string(),
        )
        .expect("ok");
        let col = &buckets[&empty_global()].models["things"].columns[1];
        assert_eq!(col.name, "medium_count");
        let check = col
            .check
            .as_deref()
            .expect("u32 → BigInt column with RustSourceType::U32 must have a range CHECK");
        assert!(
            check.contains(">= 0") && check.contains("<= 4294967295"),
            "u32 CHECK must cover 0..=4294967295: {check}"
        );
    }

    #[test]
    fn project_column_emits_check_for_u64_numeric() {
        use crate::descriptor::RustSourceType;
        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            },
            FieldDescriptor {
                rust_source_type: Some(RustSourceType::U64),
                ..field_descriptor("huge_count", FieldSqlType::Numeric, false)
            },
        ];
        let m = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("metrics", "Metric")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-05-15T00:00:00Z".to_string(),
        )
        .expect("ok");
        let col = &buckets[&empty_global()].models["metrics"].columns[1];
        assert_eq!(col.name, "huge_count");
        let check = col
            .check
            .as_deref()
            .expect("u64 → Numeric with RustSourceType::U64 must have a range+integrality CHECK");
        assert!(
            check.contains(">= 0") && check.contains("<= 18446744073709551615"),
            "u64 CHECK must cover 0..=u64::MAX: {check}"
        );
        assert!(
            check.contains("= trunc("),
            "u64 CHECK must include integrality clause (col = trunc(col)): {check}"
        );
    }

    // ── djogi#187 — temporal year-bounds projection (live wiring) ──────────
    //
    // The Date and Timestamptz arms of `field_type_check` ship active
    // — `FieldSqlType::Date` has a single Rust source type
    // (`time::Date`) and `FieldSqlType::Timestamptz` has a single Rust
    // source type (`time::OffsetDateTime`), so dispatching on
    // `FieldSqlType` alone is unambiguous and the CHECK reaches
    // `ColumnSchema.check` through `project_column`. These pin tests
    // assert the wiring stays live for adopter Date / Timestamptz
    // columns AND for the framework-injected `created_at` /
    // `updated_at` columns.

    #[test]
    fn project_column_emits_year_check_for_non_fk_date_column() {
        // Adopter `#[model] struct Product { launch_date: time::Date }`.
        // Lowers to `FieldSqlType::Date`. The projection must produce
        // a `<col> <= DATE '9999-12-31'` CHECK so external writers
        // that land OOB-upper Date values get rejected at the DB
        // layer rather than poisoning typed reads with
        // `DjogiError::Decode`. One-sided upper bound by design — see
        // `field_type_check` doc comment.
        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            },
            FieldDescriptor {
                ..field_descriptor("launch_date", FieldSqlType::Date, false)
            },
        ];
        let m = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("products", "Product")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let date_col = &buckets[&empty_global()].models["products"].columns[1];
        assert_eq!(date_col.name, "launch_date");
        let check = date_col
            .check
            .as_ref()
            .expect("DATE column must carry the time::Date year-bound CHECK (djogi#187)");
        assert!(
            check.contains("\"launch_date\" <= DATE '9999-12-31'"),
            "DATE column CHECK upper bound: {check}"
        );
    }

    #[test]
    fn project_column_emits_year_check_for_non_fk_timestamptz_column() {
        // Adopter `#[model] struct Event { occurred_at: OffsetDateTime }`.
        // Lowers to `FieldSqlType::Timestamptz`. The projection must
        // produce a year-bound CHECK matching `time::OffsetDateTime`'s
        // representable upper bound so external writers cannot land
        // OOB-upper rows.
        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            },
            FieldDescriptor {
                ..field_descriptor("occurred_at", FieldSqlType::Timestamptz, false)
            },
        ];
        let m = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("events", "Event")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let ts_col = &buckets[&empty_global()].models["events"].columns[1];
        assert_eq!(ts_col.name, "occurred_at");
        let check = ts_col.check.as_ref().expect(
            "TIMESTAMPTZ column must carry the OffsetDateTime year-bound CHECK (djogi#187)",
        );
        assert!(
            check.contains("\"occurred_at\" <= TIMESTAMPTZ '9999-12-31 23:59:59.999999+00'"),
            "TIMESTAMPTZ column CHECK upper bound: {check}"
        );
    }

    #[test]
    fn project_column_no_year_check_for_fk_column_even_if_temporal() {
        // FK columns inherit the parent PK's type — always
        // identity-width (BIGINT for HeerId-family, UUID for
        // RanjId-family). Even if a future model declared a temporal
        // PK type (not currently supported), FK columns project no
        // Rust-derived CHECK because the FK column's bounds follow
        // the parent's PK shape.
        //
        // This pin guards against a future regression where FK
        // projection accidentally inherits the CHECK string from the
        // field's `sql_type` rather than the parent's PK type.
        static OWNER_FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            ..field_descriptor("id", FieldSqlType::BigInt, false)
        }];
        // `owner_id` lowers to `Timestamptz` in this synthetic case
        // because we manually construct a descriptor with a temporal
        // relation column. The macro wouldn't emit this combination,
        // but we want to lock the invariant: FK relations never
        // receive a Rust-derived CHECK regardless of the field's
        // declared `sql_type`.
        static VEHICLE_FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            },
            FieldDescriptor {
                relation_kind: Some(crate::descriptor::RelationKind::ForeignKey),
                target_type_name: Some("Owner"),
                ..field_descriptor("owner_id", FieldSqlType::Timestamptz, false)
            },
        ];
        let owner = ModelDescriptor {
            fields: OWNER_FIELDS,
            ..synth_model("owners", "Owner")
        };
        let vehicle = ModelDescriptor {
            fields: VEHICLE_FIELDS,
            ..synth_model("vehicles", "Vehicle")
        };
        let buckets = project_from_iters(
            [&owner, &vehicle],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let owner_fk = &buckets[&empty_global()].models["vehicles"].columns[1];
        assert_eq!(owner_fk.name, "owner_id");
        assert!(
            owner_fk.foreign_key.is_some(),
            "owner_id must project as FK: {owner_fk:?}"
        );
        assert!(
            owner_fk.check.is_none(),
            "FK column must never carry a Rust-derived CHECK \
             regardless of declared sql_type: {:?}",
            owner_fk.check
        );
    }

    // ── djogi#188 — Decimal structural CHECK projection ────────────────────
    //
    // `Decimal` columns (`rust_decimal::Decimal`) lower to `FieldSqlType::Numeric`
    // and carry `Some(RustSourceType::Decimal)`. The projection emits a
    // structural CHECK bounding the value to rust_decimal's representable
    // range (96-bit mantissa, scale ≤ 28).

    #[test]
    fn field_type_check_for_decimal_numeric_emits_structural_bounds() {
        let expr = field_type_check(
            &FieldSqlType::Numeric,
            Some(RustSourceType::Decimal),
            "price",
        )
        .expect("Decimal → Numeric must carry the rust_decimal structural CHECK");
        // The scale clause caps the fractional-digit count at 28.
        assert!(
            expr.contains("scale(\"price\") <= 28"),
            "Decimal CHECK must cap scale at 28: {expr}"
        );
        // The mantissa clause keeps `|col| * 10^scale(col)` inside the
        // 96-bit unsigned range (2^96 - 1).
        assert!(
            expr.contains("abs(\"price\") * power(10::numeric, scale(\"price\"))"),
            "Decimal CHECK must scale value back to integer coefficient form: {expr}"
        );
        assert!(
            expr.contains("79228162514264337593543950335"),
            "Decimal CHECK upper-bound must be 2^96 - 1 (79228162514264337593543950335): {expr}"
        );
    }

    #[test]
    fn field_type_check_decimal_arm_quotes_reserved_word_column() {
        // The Decimal CHECK references the column five times:
        //   1. outer `({qcol}) IS NULL` pass-through wrap;
        //   2. `scale({qcol}) IS NOT NULL` (special-value guard);
        //   3. `scale({qcol}) <= 28` (scale bound);
        //   4. `abs({qcol})` (coefficient base);
        //   5. `scale({qcol})` inside `power(10::numeric, ...)` (coefficient
        //      exponent).
        // All five must round-trip the column name through
        // `quote_ident_for_check` so a reserved-word column name parses
        // cleanly.
        let expr = field_type_check(
            &FieldSqlType::Numeric,
            Some(RustSourceType::Decimal),
            "order",
        )
        .expect("Decimal CHECK must fire regardless of column name");
        assert_eq!(
            expr.matches("\"order\"").count(),
            5,
            "Decimal CHECK references the column five times; all must be quoted: {expr}"
        );
    }

    #[test]
    fn field_type_check_decimal_arm_rejects_numeric_special_values_via_scale_guard() {
        // PostgreSQL NUMERIC admits `NaN`, `Infinity`, and `-Infinity` as
        // distinct special values. `rust_decimal::Decimal` cannot represent
        // any of them, so the structural CHECK must reject them at the DB
        // layer. The leading `scale(<col>) IS NOT NULL` clause is the only
        // guard that fires on those inputs — `pg_catalog.scale()` returns
        // NULL for every non-finite NUMERIC, and the bare scale / coefficient
        // clauses NULL-propagate (which CHECK treats as PASS).
        let expr = field_type_check(
            &FieldSqlType::Numeric,
            Some(RustSourceType::Decimal),
            "price",
        )
        .expect("Decimal CHECK must carry the scale IS NOT NULL guard");
        assert!(
            expr.contains("scale(\"price\") IS NOT NULL"),
            "Decimal CHECK must carry the `scale(...) IS NOT NULL` guard to reject NaN / \
             Infinity / -Infinity: {expr}"
        );
        // The pass-through wrap keeps SQL NULL satisfied (the `scale IS NOT
        // NULL` guard alone would also reject NULL on nullable Decimal columns).
        assert!(
            expr.contains("(\"price\") IS NULL OR ("),
            "Decimal CHECK must wrap with `(<col>) IS NULL OR (...)` so nullable Decimal \
             columns are unaffected by the special-value guard: {expr}"
        );
    }

    #[test]
    fn field_type_check_for_range_num_carries_scale_guard_on_both_endpoints() {
        // The same `scale(...) IS NOT NULL` special-value guard fires on
        // NUMRANGE endpoints. `range_endpoint_checks` wraps each finite
        // endpoint with its own `IS NULL OR (...)` short-circuit, so
        // `decimal_repr_expr` itself returns the bare conjunction and the
        // wrap delivers the NULL pass-through.
        let expr = field_type_check(
            &FieldSqlType::Range {
                subtype: RangeSubtypeKind::Num,
            },
            None,
            "price_range",
        )
        .expect("NUMRANGE must carry endpoint Decimal CHECKs");
        assert!(
            expr.contains("scale(lower(\"price_range\")) IS NOT NULL"),
            "NUMRANGE lower endpoint must carry the special-value guard: {expr}"
        );
        assert!(
            expr.contains("scale(upper(\"price_range\")) IS NOT NULL"),
            "NUMRANGE upper endpoint must carry the special-value guard: {expr}"
        );
    }

    #[test]
    fn project_column_emits_decimal_structural_check_for_non_fk_numeric_column() {
        // Adopter `pub price: Decimal` → `FieldSqlType::Numeric` with
        // `rust_source_type: Some(RustSourceType::Decimal)`. The
        // projection must produce the structural CHECK so external
        // writers cannot land values outside rust_decimal's
        // representable range and corrupt typed `FromSql` reads with
        // `DjogiError::Decode`.
        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            },
            FieldDescriptor {
                rust_source_type: Some(RustSourceType::Decimal),
                ..field_descriptor("price", FieldSqlType::Numeric, false)
            },
        ];
        let m = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("products", "Product")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-05-16T00:00:00Z".to_string(),
        )
        .expect("ok");
        let price_col = &buckets[&empty_global()].models["products"].columns[1];
        assert_eq!(price_col.name, "price");
        let check = price_col
            .check
            .as_deref()
            .expect("Decimal column with RustSourceType::Decimal must carry the structural CHECK");
        assert!(
            check.contains("scale(\"price\") <= 28"),
            "Decimal CHECK must cap scale at 28: {check}"
        );
        assert!(
            check.contains("79228162514264337593543950335"),
            "Decimal CHECK must reference 2^96 - 1 as upper coefficient bound: {check}"
        );
    }

    #[test]
    fn field_type_check_for_range_num_emits_endpoint_decimal_bounds() {
        let expr = field_type_check(
            &FieldSqlType::Range {
                subtype: RangeSubtypeKind::Num,
            },
            None,
            "price_range",
        )
        .expect("NUMRANGE must carry Decimal bound checks on finite lower and upper endpoints");
        assert!(
            expr.contains("scale(lower(\"price_range\")) <= 28"),
            "NUMRANGE lower endpoint must get Decimal scale bound: {expr}"
        );
        assert!(
            expr.contains("scale(upper(\"price_range\")) <= 28"),
            "NUMRANGE upper endpoint must get Decimal scale bound: {expr}"
        );
        assert!(
            expr.contains("scale(lower(\"price_range\")) <= 28 AND abs(lower(\"price_range\")) * power(10::numeric, scale(lower(\"price_range\"))) <= 79228162514264337593543950335"),
            "NUMRANGE lower endpoint should reuse Decimal element checks: {expr}"
        );
        assert!(
            expr.contains("scale(upper(\"price_range\")) <= 28 AND abs(upper(\"price_range\")) * power(10::numeric, scale(upper(\"price_range\"))) <= 79228162514264337593543950335"),
            "NUMRANGE upper endpoint should reuse Decimal element checks: {expr}"
        );
        assert!(
            expr.contains("lower(\"price_range\") IS NULL OR"),
            "NUMRANGE lower endpoint check must keep NULL/unbounded pass-through: {expr}"
        );
        assert!(
            expr.contains("upper(\"price_range\") IS NULL OR"),
            "NUMRANGE upper endpoint check must keep NULL/unbounded pass-through: {expr}"
        );
    }

    #[test]
    fn field_type_check_for_range_tstz_emits_endpoint_timestamptz_bounds() {
        let expr = field_type_check(
            &FieldSqlType::Range {
                subtype: RangeSubtypeKind::Tstz,
            },
            None,
            "booking_window",
        )
        .expect(
            "TSTZRANGE must carry Timestamptz upper checks on finite lower and upper endpoints",
        );
        assert!(
            expr.contains(
                "lower(\"booking_window\") <= TIMESTAMPTZ '9999-12-31 23:59:59.999999+00'"
            ),
            "TSTZRANGE lower endpoint bound must be UTC-explicit TIMESTAMPTZ: {expr}"
        );
        assert!(
            expr.contains(
                "upper(\"booking_window\") <= TIMESTAMPTZ '9999-12-31 23:59:59.999999+00'"
            ),
            "TSTZRANGE upper endpoint bound must be UTC-explicit TIMESTAMPTZ: {expr}"
        );
        assert!(
            expr.contains("lower(\"booking_window\") IS NULL OR"),
            "TSTZRANGE lower endpoint check must keep NULL/unbounded pass-through: {expr}"
        );
        assert!(
            expr.contains("upper(\"booking_window\") IS NULL OR"),
            "TSTZRANGE upper endpoint check must keep NULL/unbounded pass-through: {expr}"
        );
    }

    #[test]
    fn field_type_check_for_range_date_emits_endpoint_date_bounds() {
        let expr = field_type_check(
            &FieldSqlType::Range {
                subtype: RangeSubtypeKind::Date,
            },
            None,
            "validity",
        )
        .expect("DATERANGE must carry Date upper checks on finite lower and upper endpoints");
        assert!(
            expr.contains("lower(\"validity\") <= DATE '9999-12-31'"),
            "DATERANGE lower endpoint bound must be finite upper check: {expr}"
        );
        assert!(
            expr.contains("upper(\"validity\") <= DATE '9999-12-31'"),
            "DATERANGE upper endpoint bound must be finite upper check: {expr}"
        );
        assert!(
            expr.contains("lower(\"validity\") IS NULL OR"),
            "DATERANGE lower endpoint check must keep NULL/unbounded pass-through: {expr}"
        );
        assert!(
            expr.contains("upper(\"validity\") IS NULL OR"),
            "DATERANGE upper endpoint check must keep NULL/unbounded pass-through: {expr}"
        );
    }

    #[test]
    fn field_type_check_for_range_int4_is_noop() {
        assert!(
            field_type_check(
                &FieldSqlType::Range {
                    subtype: RangeSubtypeKind::Int4,
                },
                None,
                "slot",
            )
            .is_none(),
            "INT4RANGE has no projection CHECK (identity-mapped i32 bounds)"
        );
    }

    #[test]
    fn field_type_check_for_range_int8_is_noop() {
        assert!(
            field_type_check(
                &FieldSqlType::Range {
                    subtype: RangeSubtypeKind::Int8,
                },
                None,
                "slot",
            )
            .is_none(),
            "INT8RANGE has no projection CHECK (identity-mapped i64 bounds)"
        );
    }

    #[test]
    fn field_type_check_for_timestamptz_array_elements_enforces_representability() {
        let expr = field_type_check(&FieldSqlType::TimestamptzArray, None, "slots")
            .expect("TIMESTAMPTZ[] must carry per-element representability checks");
        assert!(
            expr.contains("\"slots\" IS NULL OR"),
            "TIMESTAMPTZ[] outer NULL should pass through: {expr}"
        );
        assert!(
            expr.contains("TIMESTAMPTZ '9999-12-31 23:59:59.999999+00' >= ALL(\"slots\")"),
            "TIMESTAMPTZ[] check should use CHECK-valid ALL bounds: {expr}"
        );
        assert!(
            !expr.contains("NOT EXISTS (SELECT 1 FROM unnest(\"slots\")"),
            "TIMESTAMPTZ[] check should not emit a subquery CHECK: {expr}"
        );
        assert!(
            expr.contains("TIMESTAMPTZ '9999-12-31 23:59:59.999999+00' >= ALL(\"slots\")"),
            "TIMESTAMPTZ[] check should reuse scalar temporal upper-bound policy: {expr}"
        );
    }

    #[test]
    fn field_type_check_for_date_array_elements_enforces_representability() {
        let expr = field_type_check(&FieldSqlType::DateArray, None, "validity")
            .expect("DATE[] must carry per-element representability checks");
        assert!(
            expr.contains("\"validity\" IS NULL OR"),
            "DATE[] outer NULL should pass through: {expr}"
        );
        assert!(
            expr.contains("DATE '9999-12-31' >= ALL(\"validity\")"),
            "DATE[] check should use CHECK-valid ALL bounds: {expr}"
        );
        assert!(
            !expr.contains("NOT EXISTS (SELECT 1 FROM unnest(\"validity\")"),
            "DATE[] check should not emit a subquery CHECK: {expr}"
        );
        assert!(
            expr.contains("DATE '9999-12-31' >= ALL(\"validity\")"),
            "DATE[] check should reuse scalar temporal upper-bound policy: {expr}"
        );
    }

    #[test]
    fn field_type_check_for_decimal_array_elements_enforces_representability() {
        let expr = field_type_check(&FieldSqlType::NumericArray, None, "metrics")
            .expect("NUMERIC[] must carry per-element representability checks");
        assert!(
            expr.contains("\"metrics\" IS NULL OR"),
            "NUMERIC[] outer NULL should pass through: {expr}"
        );
        assert!(
            expr.contains("djogi.__djogi_numeric_array_is_rust_decimal_v1(\"metrics\")"),
            "NUMERIC[] check should use helper-backed representability CHECK: {expr}"
        );
        assert!(
            !expr.contains("NOT EXISTS (SELECT 1 FROM unnest(\"metrics\")"),
            "NUMERIC[] check should not emit a subquery CHECK: {expr}"
        );
        assert!(
            !expr.contains("scale(element)"),
            "NUMERIC[] check should centralize decimal logic in helper: {expr}"
        );
        assert!(
            expr.contains("djogi.__djogi_numeric_array_is_rust_decimal_v1(\"metrics\")"),
            "NUMERIC[] check should reuse scalar decimal representability policy: {expr}"
        );
    }

    #[test]
    fn project_column_emits_range_endpoint_checks_and_noops_for_int_ranged() {
        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            },
            FieldDescriptor {
                ..field_descriptor(
                    "slots",
                    FieldSqlType::Range {
                        subtype: RangeSubtypeKind::Int4,
                    },
                    false,
                )
            },
            FieldDescriptor {
                ..field_descriptor(
                    "money",
                    FieldSqlType::Range {
                        subtype: RangeSubtypeKind::Num,
                    },
                    false,
                )
            },
            FieldDescriptor {
                ..field_descriptor(
                    "window",
                    FieldSqlType::Range {
                        subtype: RangeSubtypeKind::Tstz,
                    },
                    false,
                )
            },
            FieldDescriptor {
                ..field_descriptor(
                    "validity",
                    FieldSqlType::Range {
                        subtype: RangeSubtypeKind::Date,
                    },
                    false,
                )
            },
        ];
        let m = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("offers", "Offer")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-05-17T00:00:00Z".to_string(),
        )
        .expect("ok");
        let rows = &buckets[&empty_global()].models["offers"].columns;
        let slots = &rows[1];
        let money = &rows[2];
        let window = &rows[3];
        let validity = &rows[4];

        assert!(slots.check.is_none(), "INT4RANGE should stay no-op");
        assert!(
            money
                .check
                .as_deref()
                .expect("NUMRANGE must carry endpoint checks")
                .contains("lower(\"money\") IS NULL OR (scale(lower(\"money\")) IS NOT NULL AND scale(lower(\"money\")) <= 28"),
            "NUMRANGE lower endpoint must use DECIMAL element check with special-value guard"
        );
        assert!(
            money
                .check
                .as_deref()
                .expect("NUMRANGE must carry endpoint checks")
                .contains("upper(\"money\") IS NULL OR (scale(upper(\"money\")) IS NOT NULL AND scale(upper(\"money\")) <= 28"),
            "NUMRANGE upper endpoint must use DECIMAL element check with special-value guard"
        );
        assert!(
            window.check.as_deref().expect("TSTZRANGE must carry endpoint checks").contains(
                "lower(\"window\") IS NULL OR (lower(\"window\") <= TIMESTAMPTZ '9999-12-31 23:59:59.999999+00'"
            ),
            "TSTZRANGE lower endpoint must use TIMESTAMPTZ upper bound"
        );
        assert!(
            window
                .check
                .as_deref()
                .expect("TSTZRANGE must carry endpoint checks")
                .contains("upper(\"window\") IS NULL OR (upper(\"window\") <= TIMESTAMPTZ '9999-12-31 23:59:59.999999+00'"),
            "TSTZRANGE upper endpoint must use TIMESTAMPTZ upper bound"
        );
        assert!(
            validity
                .check
                .as_deref()
                .expect("DATERANGE must carry endpoint checks")
                .contains(
                    "lower(\"validity\") IS NULL OR (lower(\"validity\") <= DATE '9999-12-31'"
                ),
            "DATERANGE lower endpoint must use DATE upper bound"
        );
        assert!(
            validity
                .check
                .as_deref()
                .expect("DATERANGE must carry endpoint checks")
                .contains(
                    "upper(\"validity\") IS NULL OR (upper(\"validity\") <= DATE '9999-12-31'"
                ),
            "DATERANGE upper endpoint must use DATE upper bound"
        );
    }

    #[test]
    fn project_column_emits_array_element_representability_checks() {
        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            },
            FieldDescriptor {
                ..field_descriptor("slots", FieldSqlType::TimestamptzArray, false)
            },
            FieldDescriptor {
                ..field_descriptor("validity", FieldSqlType::DateArray, false)
            },
            FieldDescriptor {
                ..field_descriptor("metrics", FieldSqlType::NumericArray, false)
            },
        ];
        let m = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("offers", "Offer")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-05-17T00:00:00Z".to_string(),
        )
        .expect("ok");
        let rows = &buckets[&empty_global()].models["offers"].columns;
        let slots = &rows[1];
        let validity = &rows[2];
        let metrics = &rows[3];

        let slot_check = slots
            .check
            .as_deref()
            .expect("TIMESTAMPTZ[] column must carry per-element representability check");
        let validity_check = validity
            .check
            .as_deref()
            .expect("DATE[] column must carry per-element representability check");
        let metrics_check = metrics
            .check
            .as_deref()
            .expect("NUMERIC[] column must carry per-element representability check");

        assert!(
            slot_check.contains("TIMESTAMPTZ '9999-12-31 23:59:59.999999+00' >= ALL(\"slots\")"),
            "TIMESTAMPTZ[] projection should use CHECK-valid ALL bound: {slot_check}"
        );
        assert!(
            validity_check.contains("DATE '9999-12-31' >= ALL(\"validity\")"),
            "DATE[] projection should use CHECK-valid ALL bound: {validity_check}"
        );
        assert!(
            metrics_check.contains("djogi.__djogi_numeric_array_is_rust_decimal_v1(\"metrics\")"),
            "NUMERIC[] projection should use helper-backed check: {metrics_check}"
        );
        assert!(
            !metrics_check.contains("element"),
            "NUMERIC[] projection should not emit per-element aliases: {metrics_check}"
        );
    }

    // ── djogi#105 — adopter `#[field(check = "...")]` projection ───────────
    //
    // The macro emits `FieldDescriptor::check_sql` from the parsed
    // `#[field(check = "...")]` attribute. The projection layer combines
    // it with any type-derived CHECK via logical `AND` into a single
    // constraint slot.

    #[test]
    fn combine_check_expressions_neither_present_returns_none() {
        assert!(combine_check_expressions(None, None).is_none());
    }

    #[test]
    fn combine_check_expressions_only_type_derived() {
        let combined = combine_check_expressions(Some("\"qty\" >= 0".into()), None);
        assert_eq!(combined.as_deref(), Some("\"qty\" >= 0"));
    }

    #[test]
    fn combine_check_expressions_only_adopter() {
        let combined = combine_check_expressions(None, Some("weight_kg > 0"));
        assert_eq!(combined.as_deref(), Some("weight_kg > 0"));
    }

    #[test]
    fn combine_check_expressions_both_present_combines_with_and() {
        let combined = combine_check_expressions(
            Some("\"port\" >= 0 AND \"port\" <= 65535".into()),
            Some("port <> 0"),
        );
        assert_eq!(
            combined.as_deref(),
            Some("(\"port\" >= 0 AND \"port\" <= 65535) AND (port <> 0)")
        );
    }

    #[test]
    fn combine_check_expressions_trims_whitespace_for_stable_diff() {
        // Snapshot comparison is byte-equality; any whitespace drift
        // between projection runs would emit a spurious AMEND.
        let combined =
            combine_check_expressions(Some("  type_clause  ".into()), Some("   adopter_clause   "));
        assert_eq!(
            combined.as_deref(),
            Some("(type_clause) AND (adopter_clause)")
        );
    }

    #[test]
    fn project_column_propagates_adopter_check_sql_only() {
        // Plain column with no type-derived CHECK — adopter
        // `#[field(check = "weight_kg > 0")]` becomes the single
        // CHECK expression in the projected ColumnSchema.
        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            },
            FieldDescriptor {
                check_sql: Some("weight_kg > 0"),
                ..field_descriptor("weight_kg", FieldSqlType::DoublePrecision, false)
            },
        ];
        let m = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("animals", "Animal")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-05-16T00:00:00Z".to_string(),
        )
        .expect("ok");
        let col = &buckets[&empty_global()].models["animals"].columns[1];
        assert_eq!(col.name, "weight_kg");
        assert_eq!(
            col.check.as_deref(),
            Some("weight_kg > 0"),
            "adopter #[field(check)] on a DoublePrecision column lands verbatim"
        );
    }

    #[test]
    fn project_column_combines_type_check_and_adopter_check_on_u32() {
        // u32 column with adopter `#[field(check = "port > 0")]`.
        // The combined CHECK reflects both clauses; both must pass.
        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            },
            FieldDescriptor {
                rust_source_type: Some(RustSourceType::U32),
                check_sql: Some("port > 0"),
                ..field_descriptor("port", FieldSqlType::BigInt, false)
            },
        ];
        let m = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("listeners", "Listener")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-05-16T00:00:00Z".to_string(),
        )
        .expect("ok");
        let col = &buckets[&empty_global()].models["listeners"].columns[1];
        let check = col
            .check
            .as_deref()
            .expect("u32 column with adopter check must carry the combined type+adopter CHECK");
        // Combined shape: `(<u32 range>) AND (<adopter>)`.
        assert!(
            check.contains("\"port\" >= 0 AND \"port\" <= 4294967295"),
            "combined CHECK must include the u32 range bound: {check}"
        );
        assert!(
            check.contains("port > 0"),
            "combined CHECK must include the adopter expression verbatim: {check}"
        );
        assert!(
            check.starts_with("("),
            "combined CHECK must wrap each clause in parens: {check}"
        );
        assert!(
            check.contains(") AND ("),
            "combined CHECK must AND the two clauses: {check}"
        );
    }

    #[test]
    fn project_column_combines_range_type_check_and_adopter_check() {
        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            },
            FieldDescriptor {
                check_sql: Some("window IS NOT NULL"),
                ..field_descriptor(
                    "window",
                    FieldSqlType::Range {
                        subtype: RangeSubtypeKind::Date,
                    },
                    false,
                )
            },
        ];
        let m = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("offers", "Offer")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-05-17T00:00:00Z".to_string(),
        )
        .expect("ok");
        let col = &buckets[&empty_global()].models["offers"].columns[1];
        let check = col
            .check
            .as_deref()
            .expect("range column with adopter check must carry the combined constraint");
        assert!(
            check.contains("window IS NOT NULL"),
            "combined CHECK must include adopter predicate verbatim: {check}"
        );
        assert!(
            check.contains("lower(\"window\") <= DATE '9999-12-31'"),
            "combined CHECK must include DATE endpoint bound predicate: {check}"
        );
        assert!(
            check.contains(") AND (window IS NOT NULL)"),
            "combined CHECK must merge with logical AND inside one constraint slot: {check}"
        );
    }

    #[test]
    fn project_column_combines_array_type_check_and_adopter_check() {
        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            },
            FieldDescriptor {
                check_sql: Some("CARDINALITY(\"times\") > 0"),
                ..field_descriptor("times", FieldSqlType::TimestamptzArray, false)
            },
        ];
        let m = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("offers", "Offer")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-05-17T00:00:00Z".to_string(),
        )
        .expect("ok");
        let col = &buckets[&empty_global()].models["offers"].columns[1];
        let check = col
            .check
            .as_deref()
            .expect("array column with adopter check must carry the combined constraint");
        assert!(
            check.contains("CARDINALITY(\"times\") > 0"),
            "combined CHECK should include adopter predicate verbatim: {check}"
        );
        assert!(
            check.contains("TIMESTAMPTZ '9999-12-31 23:59:59.999999+00' >= ALL(\"times\")"),
            "combined CHECK should include per-element typed check: {check}"
        );
        assert!(
            check.contains(") AND (CARDINALITY(\"times\") > 0)"),
            "combined CHECK should include logical AND between clauses: {check}"
        );
    }

    #[test]
    fn project_column_emits_adopter_check_on_fk_column() {
        // FK columns inherit the parent PK's identity-width type so
        // the type-derived CHECK is suppressed. The adopter's
        // `#[field(check = "...")]` survives — there is no structural
        // reason to forbid domain invariants on FK columns, and the
        // adopter may want one (e.g., `owner_id > 0` to reject the
        // HeerId sentinel value zero on a non-null FK column).
        static OWNER_FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            ..field_descriptor("id", FieldSqlType::BigInt, false)
        }];
        static VEHICLE_FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            },
            FieldDescriptor {
                relation_kind: Some(crate::descriptor::RelationKind::ForeignKey),
                target_type_name: Some("Owner"),
                check_sql: Some("owner_id > 0"),
                ..field_descriptor("owner_id", FieldSqlType::BigInt, false)
            },
        ];
        let owner = ModelDescriptor {
            fields: OWNER_FIELDS,
            ..synth_model("owners", "Owner")
        };
        let vehicle = ModelDescriptor {
            fields: VEHICLE_FIELDS,
            ..synth_model("vehicles", "Vehicle")
        };
        let buckets = project_from_iters(
            [&owner, &vehicle],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-05-16T00:00:00Z".to_string(),
        )
        .expect("ok");
        let owner_fk = &buckets[&empty_global()].models["vehicles"].columns[1];
        assert_eq!(owner_fk.name, "owner_id");
        assert!(
            owner_fk.foreign_key.is_some(),
            "owner_id must project as FK: {owner_fk:?}"
        );
        // Adopter CHECK survives the FK projection path.
        assert_eq!(
            owner_fk.check.as_deref(),
            Some("owner_id > 0"),
            "adopter #[field(check)] on FK column must reach ColumnSchema.check"
        );
    }

    #[test]
    fn project_column_emits_year_check_for_framework_timestamps() {
        // Framework-injected `created_at` / `updated_at` columns
        // lower to `FieldSqlType::Timestamptz` like any adopter
        // OffsetDateTime field. The temporal year CHECK applies here
        // too — Postgres `now()` always returns a current-era
        // timestamp so the CHECK is satisfied by the column DEFAULT,
        // but external writers (raw migrations, BI tools) can still
        // drift these columns. The CHECK protects against that drift.
        //
        // This test constructs an explicit `created_at` / `updated_at`
        // descriptor (matching the layout the macro would emit) and
        // verifies the CHECK reaches the projected ColumnSchema. The
        // column-name match in `project_column` for `DEFAULT now()`
        // doesn't interfere with the CHECK — the two are independent
        // column attributes.
        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            },
            FieldDescriptor {
                ..field_descriptor("created_at", FieldSqlType::Timestamptz, false)
            },
            FieldDescriptor {
                ..field_descriptor("updated_at", FieldSqlType::Timestamptz, false)
            },
        ];
        let m = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("widgets", "Widget")
        };
        let buckets = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("ok");
        let cols = &buckets[&empty_global()].models["widgets"].columns;
        let created_at = cols
            .iter()
            .find(|c| c.name == "created_at")
            .expect("explicit created_at column projected");
        let updated_at = cols
            .iter()
            .find(|c| c.name == "updated_at")
            .expect("explicit updated_at column projected");
        for col in [created_at, updated_at] {
            // Sanity check: the DEFAULT now() routing still fires on
            // the canonical timestamp column names (the year CHECK
            // doesn't displace it).
            assert_eq!(
                col.default_sql.as_deref(),
                Some("now()"),
                "{} keeps DEFAULT now() alongside the year CHECK",
                col.name
            );
            let check = col.check.as_ref().unwrap_or_else(|| {
                panic!(
                    "framework Timestamptz column {} must carry the \
                     OffsetDateTime year-bound CHECK (djogi#187)",
                    col.name
                )
            });
            assert!(
                check.contains("TIMESTAMPTZ '9999-12-31 23:59:59.999999+00'"),
                "{} CHECK upper bound must use UTC-explicit TIMESTAMPTZ form: {check}",
                col.name
            );
        }
    }
}
