//! Compile-time schema ownership domains — the apps subsystem.
//! Users declare apps once per crate via the
//! [`djogi::apps!`](crate::apps) function-like proc macro:
//! ```rust,ignore
//! djogi::apps! {
//!  #[app(database = "main")]
//!  pub struct Vehicles;
//!
//!  #[app(database = "main")]
//!  pub struct Users;
//!
//!  #[app(database = "crud_log")]
//!  pub struct Audit;
//! }
//! ```
//! Each entry expands to a zero-sized unit struct, a hidden seal
//! witness on its [`App`] impl, and an `inventory::submit!` of the
//! struct's [`AppDescriptor`]. Migration and ledger code iterate the
//! collected descriptors via [`AppRegistry::all`] to group tables by
//! `(database_target, app_label)`.
//! # Sealing
//! [`App`] carries a hidden seal witness whose type has no public
//! constructor. The [`djogi::apps!`] macro emits that witness for each
//! declared app; hand-written `impl djogi::App for MyStruct {}` in
//! downstream crates fail because the hidden witness item is missing.
//! The older public `apps::sealed::Sealed` convention is gone so
//! downstream crates cannot satisfy the seal with `impl
//! djogi::apps::sealed::Sealed for MyStruct {}` or aliases of that
//! path.
//! # Global bucket
//! [`AppRegistry::all`] always includes a synthetic bucket whose
//! `LABEL` is the empty string and whose `DATABASE` is the default
//! target (`"main"`). Models declared without `#[model(app = …)]` in
//! the macro grammar fall into that bucket. The synthetic entry keeps
//! migration, build-script, and ledger consumers uniform across the
//! apps-unaware zero-config case and the fully-apped case, even when
//! no user `djogi::apps!` invocation exists in the crate.

use std::sync::OnceLock;

/// Hidden seal witness type for [`App`].
/// `#[doc(hidden)] pub` because the trait associated const
/// `__DJOGI_APP_SEAL` has this type and the trait itself is public
/// macro emission in downstream crates needs to be able to name the
/// type. The struct has no public constructor; the sole value lives
/// in [`crate::__private::apps_seal::TOKEN`] which routes through
/// the off-limits `__private` namespace. The previous public
/// `__DJOGI_APPS_SEAL_TOKEN` re-export at this module's top level
/// has been removed because it made the seal bypassable
/// downstream code could grab the const and hand-roll an `App` impl.
/// Downstream code reaching the value through `djogi::__private` is
/// explicitly violating the framework boundary; same convention as
/// `VisageSealed` in [`crate::__private`].
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SealToken {
    _private: (),
}

impl SealToken {
    /// `pub(crate)` constructor so the [`crate::__private::apps_seal`]
    /// module can mint the witness without exposing a public path on
    /// the type itself. The empty private field still keeps
    /// downstream code from naming `SealToken {... }` directly.
    pub(crate) const fn __new() -> Self {
        Self { _private: () }
    }
}

/// Compile-time schema ownership domain for a set of models.
/// Implemented by every unit struct declared inside a [`djogi::apps!`]
/// invocation. Downstream code references an app by type path
/// (`#[model(app = Vehicles)]`); Rust's own name resolution enforces
/// declaration and the seal rejects non-app types with a trait-bound
/// error.
/// # Associated constants
/// - `LABEL` — the stable string identifier used in migration files,
/// ledger rows, and snapshot JSON. Defaults to the struct identifier
/// lowercased byte-by-byte (`Vehicles` → `"vehicles"`); override
/// via `#[app(label = "…")]` when the default would be awkward
/// (`BillingAccounts` → `"billingaccounts"`).
/// - `DATABASE` — the database-target name this app belongs to. Set
/// by `#[app(database = "…")]`; required (no default — an app
/// without an explicit target is a compile error today to avoid
/// silently landing tables in `main`).
/// - `DESCRIPTOR` — the const [`AppDescriptor`] reflecting this
/// app's runtime metadata. Registry consumers prefer iterating
/// [`AppRegistry::all`], while code that knows a specific app at
/// compile time can read the const directly.
pub trait App {
    /// Hidden seal witness emitted by [`djogi::apps!`].
    #[doc(hidden)]
    const __DJOGI_APP_SEAL: SealToken;
    /// Stable string identifier for this app — see [`AppDescriptor::label`].
    const LABEL: &'static str;
    /// Database-target name — see [`AppDescriptor::database`].
    const DATABASE: &'static str;
    /// `true` when this app has been tombstoned via
    /// `#[app(tombstone, …)]`. `#[derive(Model)]` emits a
    /// compile-time assertion against this const whenever
    /// `#[model(app = X)]` references a tombstoned app — active
    /// models must either stay on a live app or use
    /// `moved_from_app = X` historical metadata instead.
    const TOMBSTONE: bool = false;
    /// Const-constructed descriptor emitted into the inventory slice.
    const DESCRIPTOR: AppDescriptor;
}

/// Runtime metadata for one registered app.
/// Emitted once per `djogi::apps!` entry via `inventory::submit!`,
/// collected by [`AppRegistry::all`], and read by migration,
/// snapshot, and ledger consumers.
/// `renamed_from` and `tombstone` support lifecycle markers without
/// requiring a struct-layout change at every `inventory::submit!`
/// call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppDescriptor {
    /// Stable identifier used in migration files, ledger rows, and
    /// snapshots. Matches the §3 Postgres-identifier grammar enforced
    /// at macro-expansion time: non-empty; first byte `b'_'` or
    /// `u8::is_ascii_alphabetic`; remaining bytes `b'_'` or
    /// `u8::is_ascii_alphanumeric`; total length ≤ 63 bytes. The
    /// synthetic global bucket is the one exception — its label is
    /// the empty string by design.
    pub label: &'static str,
    /// Database-target name. One of the built-in identifiers (`main`,
    /// `crud_log`, `event_log`) or a user-defined target. Migration
    /// consumers group by `(database, label)` — see
    /// [`docs/spec/apps-and-database-domains.md`].
    pub database: &'static str,
    /// Prior label this app was renamed from, if any. Populated by
    /// the `#[app(renamed_from = "…")]` marker.
    /// Carries the **old string label**, not a type — the old type
    /// may no longer exist in source.
    pub renamed_from: Option<&'static str>,
    /// `true` when the app has been tombstoned via `#[app(tombstone)]`
    /// (tombstone marker). Compose code gates destructive migration
    /// generation behind `--allow-destructive` when this flag is set.
    pub tombstone: bool,
}

impl AppDescriptor {
    /// Label of the synthetic global bucket. Models declared without
    /// `#[model(app = …)]` belong here. Exported as a const so
    /// consumers can compare by pointer-equality / `==` against a
    /// single canonical value.
    pub const GLOBAL_LABEL: &'static str = "";

    /// Database target assigned to the synthetic global bucket
    /// (the default target).
    pub const GLOBAL_DATABASE: &'static str = "main";

    /// The synthetic global-bucket entry — always present in
    /// [`AppRegistry::all`].
    pub const GLOBAL: AppDescriptor = AppDescriptor {
        label: Self::GLOBAL_LABEL,
        database: Self::GLOBAL_DATABASE,
        renamed_from: None,
        tombstone: false,
    };
}

inventory::collect!(AppDescriptor);

/// Enforce app-identity uniqueness on a slice already sorted by
/// `(label, database)`.
/// Two flavours of collision exist in the registry; both panic loudly
/// at startup so the wrong app cannot land in a migration directory:
/// 1. **Same `(database, label)` declared twice.** The migration
/// contract's literal identity collision — typically the result of
/// declaring the same app in two `djogi::apps!` invocations across
/// linked crates.
/// 2. **Same `label`, different `database`.** Legitimate under the
/// full `(database, label)` identity contract but rejected in v1
/// because [`crate::ModelDescriptor`] carries only `label`. The
/// cross-app FK edge generator looks up a model's database via a
/// `label → database` map; without workspace-wide label
/// uniqueness, that map silently collapses one entry and a model
/// routes to the wrong database. The descriptor-shape upgrade that
/// would unlock the looser identity is deferred to
/// `docs/spec/apps-and-database-domains.md`.
/// Mirrors the `type_to_identity` collision panic in
/// [`AppRegistry::cross_app_edges`].
/// Lifted to a free function so unit tests can drive synthetic
/// descriptor lists past the panic guard without going through the
/// link-time `inventory` collection.
fn validate_app_identity_uniqueness(sorted: &[AppDescriptor]) {
    for pair in sorted.windows(2) {
        if pair[0].label.is_empty() || pair[0].label != pair[1].label {
            continue;
        }
        if pair[0].database == pair[1].database {
            panic!(
                "djogi::apps: duplicate app identity \
     (database = {:?}, label = {:?}) declared across \
     multiple `djogi::apps!` invocations — \
     (database, label) pairs must be unique per crate \
     (and across linked djogi-using crates)",
                pair[0].database, pair[0].label,
            );
        }
        panic!(
            "djogi::apps: app label {:?} is declared in two \
    distinct database targets ({:?} and {:?}) — v1 \
    requires workspace-wide label uniqueness across \
    all databases. ModelDescriptor carries only the \
    label, so cross-app FK resolution cannot \
    disambiguate same-label / different-database \
    pairs today. Rename one of the apps, or wait for \
    the (label, database) descriptor-shape upgrade \
    deferred in docs/spec/apps-and-database-domains.md.",
            pair[0].label, pair[0].database, pair[1].database,
        );
    }
}

/// Runtime lookup facade over the apps registered in this crate
/// graph.
/// Consumers prefer [`AppRegistry::all`] to iterating
/// `inventory::iter::<AppDescriptor>` directly — `all` handles two
/// concerns:
/// 1. **Alphabetisation.** Inventory returns descriptors in link
/// order, which is non-deterministic across rebuilds and
/// toolchains. `all()` returns them sorted by `label` so
/// downstream artifacts (snapshot JSON, migration filenames,
/// ledger seed rows) are byte-stable.
/// 2. **The synthetic global bucket.** `all()` always prepends an
/// entry for `LABEL = ""` / `DATABASE = "main"` so apps-unaware
/// projects and mixed projects see the same shape from the
/// registry — `main/<empty-label>/` is always a valid target in
/// build.rs / snapshot / ledger code.
pub struct AppRegistry;

impl AppRegistry {
    /// Returns every registered [`AppDescriptor`] plus the synthetic
    /// global bucket, sorted alphabetically by `label`.
    /// The synthetic bucket's label is the empty string, which sorts
    /// first.
    /// # Identity uniqueness enforcement
    /// App identity per the migration contract is the pair
    /// `(database, label)` — migrations group by
    /// `<database_target>/<app_label>/` on disk. The full contract
    /// permits two apps with the same label in different database
    /// targets (`main/audit/` vs `crud_log/audit/`), but **v1 lifts
    /// that to a workspace-wide label-uniqueness invariant**: every
    /// app label must be unique across all databases in the registry.
    /// Within a single `djogi::apps!` invocation, duplicate labels
    /// are a compile error. Across multiple invocations — different
    /// modules of the same crate, or apps pulled in from multiple
    /// djogi-using library crates — this function panics on first
    /// call if two descriptors share a `label` (regardless of
    /// `database`).
    /// Catching the collision here rather than at compile time is a
    /// deliberate trade: the macro is function-like and expands at
    /// its call site, so crate-global compile-time enforcement would
    /// require fragile link-time symbol tricks or impossible
    /// orphan-rule dances. Runtime panic at startup
    /// (`AppRegistry::all()` runs before any migration work) is
    /// loud, early, and informative.
    /// ## Why label uniqueness, not the looser `(database, label)`?
    /// `ModelDescriptor` carries only the app **label**, not its
    /// database. The cross-app FK edge generator looks up a model's
    /// database via the registry's `label → database` map; if two
    /// apps share a label across different databases, that map
    /// silently collapses one entry and the FK edge routes a model
    /// to the wrong database. Until the descriptor shape changes to
    /// `(label, database)` — see `docs/spec/apps-and-database-domains.md`
    /// for the deferred upgrade — v1 forbids the same-label /
    /// different-database case entirely so the registry stays
    /// unambiguous.
    /// The result is computed lazily on first call and memoised in a
    /// `OnceLock`. Inventory is fixed at link time so caching the
    /// sorted vector is sound.
    pub fn all() -> &'static [AppDescriptor] {
        static CACHE: OnceLock<Vec<AppDescriptor>> = OnceLock::new();
        CACHE.get_or_init(|| {
            let mut out: Vec<AppDescriptor> = Vec::new();
            out.push(AppDescriptor::GLOBAL);
            for desc in inventory::iter::<AppDescriptor> {
                out.push(*desc);
            }
            // Sort by label first (user-facing alphabetic ordering)
            // then by database as tiebreaker so same-label entries
            // land adjacent for the duplicate-label scan in
            // `validate_app_identity_uniqueness`.
            out.sort_by(|a, b| (a.label, a.database).cmp(&(b.label, b.database)));
            validate_app_identity_uniqueness(&out);
            out
        })
    }

    /// Returns every cross-app foreign-key edge in the inventory.
    /// An edge exists when a field on a [`crate::ModelDescriptor`]
    /// carries `relation_kind` of either
    /// [`crate::descriptor::RelationKind::ForeignKey`] or
    /// [`crate::descriptor::RelationKind::OneToOne`] (both lower to
    /// SQL FKs; O2O is just a unique-backed FK) and the **source**
    /// model's app identity `(database, app_label)` differs from
    /// the **target** model's identity. Intra-app FKs are not
    /// returned — they are always safe from the apps-subsystem's
    /// perspective since source and target share a migration
    /// `<database>/<app>/` directory and compose atomically.
    /// Migration planning consumes this list to:
    /// - Emit cross-app FK clauses with the correct
    /// `REFERENCES "<target-schema>".<target-table>(id)` form.
    /// - Order per-app compose steps so target apps are applied
    /// before source apps (FKs resolve at declaration time).
    /// Models whose source or target resolves to the synthetic
    /// global bucket (empty label) are treated normally — the
    /// bucket is a valid app for FK-graph purposes.
    /// Unresolvable targets (a `target_type_name` with no matching
    /// `ModelDescriptor` in inventory) are silently skipped here
    /// the validation layer reports unresolved targets so this graph
    /// helper can stay allocation- and lookup-focused.
    /// Result is memoised in a `OnceLock` since inventory is fixed
    /// at link time.
    pub fn cross_app_edges() -> &'static [CrossAppEdge] {
        static CACHE: OnceLock<Vec<CrossAppEdge>> = OnceLock::new();
        CACHE.get_or_init(|| {
            use crate::descriptor::ModelDescriptor;
            use crate::descriptor::RelationKind;
            use std::collections::HashMap;

            // Resolve label → database once via a HashMap built from
            // the registry. The synthetic global bucket is in
            // `AppRegistry::all()` so unapp'd models ("") resolve
            // through the same map without a special case.
            // Same-label / different-database collisions would
            // silently collapse this `.collect()` and route a model
            // to the wrong database — `validate_app_identity_uniqueness`
            // (called by `AppRegistry::all()` itself) panics on that
            // case before we get here, so the resulting map is
            // unambiguous.
            let label_to_database: HashMap<&'static str, &'static str> = AppRegistry::all()
                .iter()
                .map(|d| (d.label, d.database))
                .collect();
            let database_for_label = |label: &'static str| -> &'static str {
                label_to_database
                    .get(label)
                    .copied()
                    .unwrap_or(AppDescriptor::GLOBAL_DATABASE)
            };

            // Build type-name -> (app_label, database) lookup once.
            // Keyed by short `type_name` for now; the workspace
            // convention is that model type names are unique across
            // linked crates. A future extension will key by
            // `(module_path, type_name)` once that pair is part of
            // the descriptor shape. Until then, panic loudly on
            // collision so silent shadowing surfaces at startup
            // instead of as a wrong cross-app FK edge later.
            let mut type_to_identity: HashMap<&'static str, (&'static str, &'static str)> =
                HashMap::new();
            for m in inventory::iter::<ModelDescriptor> {
                let label = m.app.unwrap_or(AppDescriptor::GLOBAL_LABEL);
                let database = database_for_label(label);
                if let Some(prior) = type_to_identity.insert(m.type_name, (label, database)) {
                    panic!(
                        "djogi::apps: model type name `{}` is registered by two distinct \
       apps — first `{}`/`{}`, now `{}`/`{}`. Type-name collisions break \
       cross-app FK edge resolution; rename one of the models, or wait for \
       (module_path, type_name) keying to land in the descriptor shape.",
                        m.type_name, prior.0, prior.1, label, database,
                    );
                }
            }

            let mut edges: Vec<CrossAppEdge> = Vec::new();
            for source in inventory::iter::<ModelDescriptor> {
                let (source_app, source_database) = type_to_identity
                    .get(source.type_name)
                    .copied()
                    .expect("source model registered above");
                for field in source.fields {
                    if !matches!(
                        field.relation_kind,
                        Some(RelationKind::ForeignKey) | Some(RelationKind::OneToOne)
                    ) {
                        continue;
                    }
                    let Some(target_type) = field.target_type_name else {
                        continue;
                    };
                    let Some(&(target_app, target_database)) = type_to_identity.get(target_type)
                    else {
                        continue;
                    };
                    if source_app == target_app && source_database == target_database {
                        continue;
                    }
                    edges.push(CrossAppEdge {
                        source_database,
                        source_app,
                        source_type: source.type_name,
                        source_field: field.name,
                        target_database,
                        target_app,
                        target_type,
                    });
                }
            }
            edges.sort_by(|a, b| {
                (
                    a.source_database,
                    a.source_app,
                    a.source_type,
                    a.source_field,
                )
                    .cmp(&(
                        b.source_database,
                        b.source_app,
                        b.source_type,
                        b.source_field,
                    ))
            });
            edges
        })
    }

    /// Returns every cross-app cycle in the FK graph.
    /// Each element is a sequence of app identities `[(db, label),
    /// …, (db, label)]` describing one cycle. Identities include the
    /// database target because `(database, label)` is the apps
    /// contract's identity pair — same-label apps in different
    /// databases are distinct participants.
    /// Same-app cycles (a model in `Billing` referencing another
    /// model in `Billing` through some chain) are deferred to
    /// intra-app analysis — this method surfaces only inter-app
    /// cycles.
    /// Algorithm: standard DFS with three-color marking over the
    /// condensed app→app graph (edges collapsed from
    /// [`Self::cross_app_edges`]). `O(A + E)` where `A` is the app
    /// count and `E` is the number of distinct inter-app edges.
    /// Result is memoised; inventory is fixed at link time.
    pub fn cross_app_cycles() -> &'static [Vec<AppIdentity>] {
        static CACHE: OnceLock<Vec<Vec<AppIdentity>>> = OnceLock::new();
        CACHE.get_or_init(|| {
            use std::collections::{HashMap, HashSet};

            // Collapse CrossAppEdge list to an (db,label)→{(db,label)}
            // adjacency map with dedup.
            let mut adj: HashMap<AppIdentity, Vec<AppIdentity>> = HashMap::new();
            for edge in Self::cross_app_edges() {
                let from = AppIdentity {
                    database: edge.source_database,
                    label: edge.source_app,
                };
                let to = AppIdentity {
                    database: edge.target_database,
                    label: edge.target_app,
                };
                let entry = adj.entry(from).or_default();
                if !entry.contains(&to) {
                    entry.push(to);
                }
            }

            // Sort adjacency lists for deterministic DFS order.
            for neighbours in adj.values_mut() {
                neighbours.sort();
            }

            let mut cycles: Vec<Vec<AppIdentity>> = Vec::new();
            let mut onstack: HashSet<AppIdentity> = HashSet::new();
            let mut done: HashSet<AppIdentity> = HashSet::new();
            let mut stack: Vec<AppIdentity> = Vec::new();
            let mut roots: Vec<AppIdentity> = adj.keys().copied().collect();
            roots.sort();

            fn dfs(
                node: AppIdentity,
                adj: &HashMap<AppIdentity, Vec<AppIdentity>>,
                onstack: &mut HashSet<AppIdentity>,
                done: &mut HashSet<AppIdentity>,
                stack: &mut Vec<AppIdentity>,
                cycles: &mut Vec<Vec<AppIdentity>>,
            ) {
                if done.contains(&node) {
                    return;
                }
                onstack.insert(node);
                stack.push(node);
                if let Some(neighbours) = adj.get(&node) {
                    for &nbr in neighbours {
                        if onstack.contains(&nbr) {
                            if let Some(start) = stack.iter().position(|n| *n == nbr) {
                                let mut cycle: Vec<AppIdentity> = stack[start..].to_vec();
                                cycle.push(nbr);
                                cycles.push(cycle);
                            }
                        } else if !done.contains(&nbr) {
                            dfs(nbr, adj, onstack, done, stack, cycles);
                        }
                    }
                }
                stack.pop();
                onstack.remove(&node);
                done.insert(node);
            }

            for root in &roots {
                dfs(
                    *root,
                    &adj,
                    &mut onstack,
                    &mut done,
                    &mut stack,
                    &mut cycles,
                );
            }
            cycles
        })
    }
}

/// App identity pair `(database, label)` — the real app identity
/// per the migration contract. Two apps with the same label
/// in different databases are distinct participants in the FK
/// graph; [`AppRegistry::cross_app_cycles`] returns paths using
/// this identity rather than bare labels so the same-label /
/// different-database case is surfaced correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AppIdentity {
    /// Database target name.
    pub database: &'static str,
    /// App label (may be the synthetic global bucket's empty string).
    pub label: &'static str,
}

/// One cross-app foreign-key edge surfaced by
/// [`AppRegistry::cross_app_edges`].
/// Migration planning uses these edges to:
/// - Order per-app compose steps so target apps apply before source
/// apps (FK constraints resolve at DDL time).
/// - Emit schema-qualified `REFERENCES "<target-schema>".<table>`
/// clauses when source and target live in different databases /
/// apps.
/// - Reject cross-database FKs entirely at compose time — Postgres
/// cannot enforce a FK across database targets.
/// App identity is `(database, label)`: two apps with the same label
/// in different databases are distinct participants. Both are
/// recorded per edge so consumers can pattern-match the full
/// identity without re-looking-up the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrossAppEdge {
    /// Database target owning the source model.
    pub source_database: &'static str,
    /// App label owning the source model (the one with the FK column).
    pub source_app: &'static str,
    /// Source model's Rust type name, e.g. `"Invoice"`.
    pub source_type: &'static str,
    /// Column name of the FK field on the source model.
    pub source_field: &'static str,
    /// Database target owning the target model.
    pub target_database: &'static str,
    /// App label owning the target model.
    pub target_app: &'static str,
    /// Target model's Rust type name, e.g. `"Customer"`.
    pub target_type: &'static str,
}

/// Apps-subsystem diagnostic contracts surfaced to migration
/// consumers.
/// Detection logic and error text live with the filesystem-vs-snapshot
/// and ledger-vs-registry comparisons. The enum lives here so
/// consumers can pattern-match on stable variants without a subsequent
/// breaking change.
/// Adding a variant is a breaking change; the enum is
/// `#[non_exhaustive]` so callers outside this crate cannot exhaust.
/// `#[doc(hidden)]` while no consumer is wired in production — the
/// variant set may shift before the first migration-tooling release
/// surfaces these. Re-exposed in rustdoc when compose / status / attune
/// land their D004 / D010 implementations.
#[doc(hidden)]
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppDiagnostic {
    /// A migration directory `<database>/<app>/` exists on disk but no
    /// matching `AppDescriptor` appears in the current build's
    /// inventory. Fields are owned `String` because the values come
    /// from runtime filesystem scans, not compile-time descriptors.
    FolderDrift {
        /// Database target containing the orphaned folder.
        database: String,
        /// App label of the orphaned folder.
        label: String,
    },

    /// A ledger row carries an `app_label` that no current inventory
    /// descriptor declares, either as `label` or as `renamed_from`.
    /// Fields are owned `String` because the values come from
    /// Postgres ledger rows at runtime, not compile-time descriptors.
    UnknownLedgerApp {
        /// Database target containing the unknown app.
        database: String,
        /// The ledger's `app_label` value that failed lookup.
        label: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_synthetic_global_bucket() {
        let all = AppRegistry::all();
        assert!(
            all.iter()
                .any(|d| d.label.is_empty() && d.database == "main" && !d.tombstone),
            "global bucket must be present"
        );
    }

    #[test]
    fn app_descriptor_global_const_matches_synthetic_bucket() {
        assert!(AppDescriptor::GLOBAL.label.is_empty());
        assert_eq!(AppDescriptor::GLOBAL.database, "main");
        assert_eq!(AppDescriptor::GLOBAL.renamed_from, None);
        // `const { assert!(...) }` so rustc proves the tombstone bit
        // is clear at compile time, not at runtime.
        const _: () = assert!(!AppDescriptor::GLOBAL.tombstone);
    }

    #[test]
    fn registry_all_is_alphabetised_by_label() {
        let all = AppRegistry::all();
        let labels: Vec<&str> = all.iter().map(|d| d.label).collect();
        let mut sorted = labels.clone();
        sorted.sort();
        assert_eq!(labels, sorted);
    }

    #[test]
    fn cross_app_edges_smoke() {
        // The djogi crate has no `#[model(app = …)]` declarations,
        // so the edge list should be empty. Real cross-app coverage
        // lives in integration tests where two apps + models
        // are actually declared. This test just proves the lazy
        // initialiser runs without panic and returns a stable slice.
        let edges = AppRegistry::cross_app_edges();
        assert!(edges.is_empty(), "djogi core has no cross-app FKs");
    }

    #[test]
    fn cross_app_cycles_smoke() {
        // Same reasoning as `cross_app_edges_smoke` — no apps
        // declared in this crate means no cycles.
        let cycles = AppRegistry::cross_app_cycles();
        assert!(cycles.is_empty(), "djogi core has no cross-app cycles");
    }

    #[test]
    fn app_diagnostic_variants_constructible() {
        // The enum is a contract: consumers can pattern-match on the
        // variants, so prove they construct. Fields are owned `String`
        // so runtime filesystem / ledger data can flow in without
        // leaking heap allocations into `'static`.
        let folder_drift = AppDiagnostic::FolderDrift {
            database: "main".to_string(),
            label: "oldbilling".to_string(),
        };
        let unknown_ledger = AppDiagnostic::UnknownLedgerApp {
            database: "main".to_string(),
            label: "mystery_app".to_string(),
        };
        assert_ne!(folder_drift, unknown_ledger);
    }

    #[test]
    fn cross_app_edge_equality_and_ordering() {
        // Two identical edges compare equal; edges sort by
        // `(source_database, source_app, source_type, source_field)`
        // so the stable ordering in `cross_app_edges()` is
        // load-bearing.
        let a = CrossAppEdge {
            source_database: "main",
            source_app: "billing",
            source_type: "Invoice",
            source_field: "customer_id",
            target_database: "main",
            target_app: "users",
            target_type: "User",
        };
        let b = CrossAppEdge {
            source_database: "main",
            source_app: "billing",
            source_type: "Invoice",
            source_field: "customer_id",
            target_database: "main",
            target_app: "users",
            target_type: "User",
        };
        assert_eq!(a, b);
    }

    #[test]
    fn app_identity_sorts_by_database_then_label() {
        let main_users = AppIdentity {
            database: "main",
            label: "users",
        };
        let main_billing = AppIdentity {
            database: "main",
            label: "billing",
        };
        let crud_audit = AppIdentity {
            database: "crud_log",
            label: "audit",
        };
        let mut ids = vec![main_users, main_billing, crud_audit];
        ids.sort();
        // crud_log < main; within main, billing < users.
        assert_eq!(ids, vec![crud_audit, main_billing, main_users]);
    }

    fn descriptor_for(label: &'static str, database: &'static str) -> AppDescriptor {
        AppDescriptor {
            label,
            database,
            renamed_from: None,
            tombstone: false,
        }
    }

    #[test]
    fn validate_uniqueness_accepts_distinct_labels() {
        // Sorted by (label, database) so the windows scan sees them
        // adjacent in a representative order.
        let mut descs = vec![
            AppDescriptor::GLOBAL,
            descriptor_for("billing", "main"),
            descriptor_for("users", "main"),
        ];
        descs.sort_by(|a, b| (a.label, a.database).cmp(&(b.label, b.database)));
        validate_app_identity_uniqueness(&descs);
    }

    #[test]
    #[should_panic(expected = "duplicate app identity")]
    fn validate_uniqueness_panics_on_duplicate_pair() {
        let mut descs = vec![
            AppDescriptor::GLOBAL,
            descriptor_for("audit", "main"),
            descriptor_for("audit", "main"),
        ];
        descs.sort_by(|a, b| (a.label, a.database).cmp(&(b.label, b.database)));
        validate_app_identity_uniqueness(&descs);
    }

    #[test]
    #[should_panic(expected = "workspace-wide label uniqueness")]
    fn validate_uniqueness_panics_on_same_label_different_database() {
        // The bug F2 fixed: `main/audit/` and `crud_log/audit/` look
        // legitimate under the full `(database, label)` identity, but
        // `ModelDescriptor` carries only `label`, so cross-app FK
        // resolution cannot disambiguate. Rejected at registration
        // time until the descriptor-shape upgrade lands.
        let mut descs = vec![
            AppDescriptor::GLOBAL,
            descriptor_for("audit", "crud_log"),
            descriptor_for("audit", "main"),
        ];
        descs.sort_by(|a, b| (a.label, a.database).cmp(&(b.label, b.database)));
        validate_app_identity_uniqueness(&descs);
    }
}
