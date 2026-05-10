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
    ModelDescriptor, PartitionSpec, PkType,
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
        tenant_key: m.tenant_key.map(|s| s.to_string()),
    }
}

fn project_fts_column(fts: &FtsDescriptor) -> ColumnSchema {
    ColumnSchema {
        check: None,
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
/// Returns `Some(expr)` for the integer-widening cases that motivate
/// `djogi#186`:
///
/// | Rust | Postgres column | CHECK expression                   |
/// |------|-----------------|-------------------------------------|
/// | `i8` | `SMALLINT`      | `<col> >= -128 AND <col> <= 127`    |
/// | `u32`| `BIGINT`        | `<col> >= 0 AND <col> <= 4294967295`|
///
/// (`u8`, `u16`, `u64` are pre-wired here for `djogi#190` — once
/// tokio-postgres bind/decode shims land, the descriptor will start
/// surfacing those types and the matching arms below trigger
/// automatically.)
///
/// The expression references the column by its quoted name so
/// identifiers using reserved words round-trip cleanly through
/// Postgres parsing. The descriptor's `name` field is the user's Rust
/// ident with `r#` stripped, which matches the column-name convention
/// the rest of the projection / SQL emitter uses.
fn field_type_check(
    sql_type: &crate::descriptor::FieldSqlType,
    column_name: &str,
) -> Option<String> {
    use crate::descriptor::FieldSqlType;

    let qcol = quote_ident_for_check(column_name);
    match sql_type {
        // Note: this match dispatches on the descriptor's typed
        // `FieldSqlType`, not on the SQL string. The macro's
        // `rust_type_to_sql` table already routes `i8 → "SMALLINT"`
        // and `u8 → "SMALLINT"` to the same Postgres column type, so
        // we can't recover the Rust source from the column type alone.
        // Today the only `SMALLINT`-producing Rust type with a CHECK is
        // `i8` — every other `SMALLINT` source (`i16`) is identity and
        // produces no CHECK. The `u8 → SMALLINT` arm activates only
        // once djogi#190 wires the macro's `rust_type_to_sql` for `u8`.
        // The CHECK projection at that point becomes ambiguous on
        // `SMALLINT` alone; the resolution is to add a typed
        // discriminant on `FieldDescriptor` (a `rust_source_type` slot)
        // so the projection can choose between `i8` and `u8` bounds.
        // For now, only `i8` is reachable, and the chosen bound matches
        // its range.
        FieldSqlType::SmallInt => Some(format!("{qcol} >= -128 AND {qcol} <= 127")),
        // `u32` is the only Rust type today that lowers to BIGINT but
        // requires a non-default range. `i64 → BIGINT` is identity and
        // produces no CHECK; the macro routes `i64` → `FieldSqlType::BigInt`
        // unconditionally. Same ambiguity caveat as `SMALLINT` above
        // applies once `i64` and `u32` land in the same projection
        // pipeline — resolved by future `rust_source_type` discriminant.
        FieldSqlType::BigInt => Some(format!("{qcol} >= 0 AND {qcol} <= 4294967295")),
        // `u8 → SMALLINT` (range 0..255), `u16 → INTEGER` (range
        // 0..65535), `u64 → NUMERIC(20, 0)` (range 0..18446744073709551615).
        // Pre-wired for djogi#190; the matching arms in
        // `rust_type_to_sql` are gated on the bind/decode shim work.
        FieldSqlType::Integer => None,
        FieldSqlType::NumericPrecision {
            precision: 20,
            scale: 0,
        } => Some(format!(
            "{qcol} >= 0 AND {qcol} <= 18446744073709551615"
        )),
        // All other `FieldSqlType` variants (`Text`, `Real`,
        // `DoublePrecision`, `Boolean`, `Timestamptz`, `Date`,
        // `Numeric`, `Uuid`, `Jsonb`, arrays, `Citext`, `Geography`,
        // `Custom`) carry their own type bounds via the column type
        // itself; no Rust-derived CHECK applies. Future families
        // (temporal year bounds — djogi#187, Decimal precision —
        // djogi#188) plug into this same match without reshaping the
        // helper signature.
        _ => None,
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

    // Type-derived CHECK projection (djogi#186, Phase 8.5 v3 Cluster 2).
    // For columns whose Rust type widens to a Postgres type (i8, u32, and
    // the eventual u8 / u16 / u64 once djogi#190 lands), we emit a CHECK
    // constraint bounding the widened column to the Rust type's value
    // range. This means external writers (raw SQL migrations, BI tools,
    // sister applications) cannot land out-of-bounds rows that would
    // later fail to decode through the typed `SELECT` path.
    //
    // FK columns and identity-mapped columns (`i16 → SMALLINT`, `i32 →
    // INTEGER`, `i64 → BIGINT`) get no CHECK — the column type already
    // covers their full range, and a redundant CHECK on every adopter
    // table would inflate the wire-format snapshot for no safety win.
    //
    // The CHECK expression references the column by its quoted name so
    // identifiers with reserved words or mixed case (the descriptor's
    // `name` field is the user-supplied Rust ident with `r#` stripped,
    // matching the column-name convention from `from_row.rs` and
    // `descriptor.rs`) round-trip cleanly. `quote_ident` lives in
    // `migrate/sql.rs`; we duplicate the rule here because the projection
    // layer cannot depend back on the SQL emitter.
    let check = if f.relation_kind.is_some() {
        // FK columns inherit their type from the parent's PK, which is
        // always identity-width (BIGINT for HeerId, UUID for RanjId, ...).
        // No type-derived CHECK applies to references.
        None
    } else {
        field_type_check(&f.sql_type, f.name)
    };

    ColumnSchema {
        check,
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
        IndexTarget, IndexType, ModelDescriptor, PkType, field_descriptor, model_descriptor,
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

    // ── djogi#186 — type-derived CHECK projection ──────────────────────────
    //
    // `field_type_check` is the projection helper that bounds widened
    // integer columns to the Rust source type's representable range.
    // The expected expression for each currently-shipped Rust type
    // (i8 → SMALLINT, u32 → BIGINT) is pinned here so a future refactor
    // cannot silently change the bound. The pre-wired arms (u64 →
    // NUMERIC(20, 0)) are also pinned so when djogi#190 surfaces u64,
    // the projection emits the documented bound without further code
    // changes.

    #[test]
    fn field_type_check_for_smallint_emits_i8_bounds() {
        let expr = field_type_check(&FieldSqlType::SmallInt, "byte_count")
            .expect("SMALLINT field must carry the i8 type-derived CHECK");
        assert!(
            expr.contains("\"byte_count\" >= -128 AND \"byte_count\" <= 127"),
            "unexpected SMALLINT CHECK expression: {expr}"
        );
    }

    #[test]
    fn field_type_check_for_bigint_emits_u32_bounds() {
        let expr = field_type_check(&FieldSqlType::BigInt, "medium_count")
            .expect("BIGINT field must carry the u32 type-derived CHECK");
        assert!(
            expr.contains("\"medium_count\" >= 0 AND \"medium_count\" <= 4294967295"),
            "unexpected BIGINT CHECK expression: {expr}"
        );
    }

    #[test]
    fn field_type_check_for_numeric_20_0_emits_u64_bounds() {
        let expr = field_type_check(
            &FieldSqlType::NumericPrecision {
                precision: 20,
                scale: 0,
            },
            "huge_count",
        )
        .expect("NUMERIC(20, 0) field must carry the u64 type-derived CHECK");
        assert!(
            expr.contains("\"huge_count\" >= 0 AND \"huge_count\" <= 18446744073709551615"),
            "unexpected NUMERIC(20, 0) CHECK expression: {expr}"
        );
    }

    #[test]
    fn field_type_check_quotes_reserved_word_columns() {
        // `order` is a Postgres reserved word — must come back quoted.
        let expr = field_type_check(&FieldSqlType::SmallInt, "order")
            .expect("SMALLINT field must carry CHECK regardless of column name");
        assert!(
            expr.contains("\"order\""),
            "CHECK expression must quote reserved-word column names: {expr}"
        );
    }

    #[test]
    fn field_type_check_returns_none_for_identity_widths() {
        // Identity-mapped Rust types (`i16 → SMALLINT` carries no
        // `i8`-derived CHECK; the projection cannot tell them apart on
        // the typed `FieldSqlType` alone today, so the chosen rule is
        // "if the type is SMALLINT, project the narrower bound" — see
        // the in-source comment on `field_type_check`. The
        // identity-width arms that genuinely have no CHECK are TEXT,
        // BOOLEAN, REAL, DOUBLE PRECISION, TIMESTAMPTZ, DATE, NUMERIC
        // (unbounded), UUID, JSONB, and the array variants.
        for ty in [
            FieldSqlType::Text,
            FieldSqlType::Boolean,
            FieldSqlType::Real,
            FieldSqlType::DoublePrecision,
            FieldSqlType::Timestamptz,
            FieldSqlType::Date,
            FieldSqlType::Numeric,
            FieldSqlType::Uuid,
            FieldSqlType::Jsonb,
            FieldSqlType::TextArray,
            FieldSqlType::IntegerArray,
            FieldSqlType::BigIntArray,
            FieldSqlType::BoolArray,
            FieldSqlType::Citext,
        ] {
            assert!(
                field_type_check(&ty, "col").is_none(),
                "non-widened SQL type {ty:?} must not carry a Rust-derived CHECK",
            );
        }
        // INTEGER is reserved for the future u16 case (djogi#190);
        // currently unreachable, so no CHECK projects either.
        assert!(
            field_type_check(&FieldSqlType::Integer, "col").is_none(),
            "INTEGER without u16 source carries no CHECK today",
        );
    }
}
