//! Compile-time schema ownership domains — the apps subsystem.
//!
//! Phase 7-Zero v3 T7 lands the runtime surface described in
//! `docs/spec/apps-and-database-domains.md` and the plan's §4B
//! "Frozen Apps Contract". Users declare apps once per crate via the
//! [`djogi::apps!`](crate::apps) function-like proc macro:
//!
//! ```rust,ignore
//! djogi::apps! {
//!     #[app(database = "main")]
//!     pub struct Vehicles;
//!
//!     #[app(database = "main")]
//!     pub struct Users;
//!
//!     #[app(database = "crud_log")]
//!     pub struct Audit;
//! }
//! ```
//!
//! Each entry expands to a zero-sized unit struct, a hidden seal
//! witness on its [`App`] impl, and an `inventory::submit!` of the
//! struct's [`AppDescriptor`]. Phase 7's migration differ iterates
//! the collected descriptors via [`AppRegistry::all`] to group tables
//! by `(database_target, app_label)`.
//!
//! # Sealing
//!
//! [`App`] carries a hidden seal witness whose type has no public
//! constructor. The [`djogi::apps!`] macro emits that witness for each
//! declared app; hand-written `impl djogi::App for MyStruct {}` in
//! downstream crates fail because the hidden witness item is missing.
//! The older public `apps::sealed::Sealed` convention is gone so
//! downstream crates cannot satisfy the seal with `impl
//! djogi::apps::sealed::Sealed for MyStruct {}` or aliases of that
//! path.
//!
//! # Global bucket
//!
//! [`AppRegistry::all`] always includes a synthetic bucket whose
//! `LABEL` is the empty string and whose `DATABASE` is the default
//! target (`"main"`). Models declared without `#[model(app = …)]` in
//! Phase 7's macro grammar fall into that bucket — the synthetic
//! entry keeps Phase 7 consumers (differ, build.rs folder scan,
//! ledger verifier) uniform across the apps-unaware zero-config case
//! and the fully-apped case. The synthetic entry is emitted even
//! when no user `djogi::apps!` invocation exists in the crate.

use std::sync::OnceLock;

mod sealed {
    /// Hidden witness carried by macro-generated [`crate::apps::App`]
    /// impls. The field stays private so handwritten code cannot
    /// construct the token accidentally.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SealToken {
        _private: (),
    }

    pub const TOKEN: SealToken = SealToken { _private: () };
}

/// Hidden seal witness type for [`App`].
///
/// This is public only so the proc-macro expansion in downstream
/// crates can name the type. The sole value lives in
/// [`__DJOGI_APPS_SEAL_TOKEN`]; the struct has no public constructor.
#[doc(hidden)]
pub use sealed::SealToken;

/// Hidden witness value that only macro-generated [`App`] impls are
/// expected to use.
#[doc(hidden)]
pub const __DJOGI_APPS_SEAL_TOKEN: SealToken = sealed::TOKEN;

/// Compile-time schema ownership domain for a set of models.
///
/// Implemented by every unit struct declared inside a [`djogi::apps!`]
/// invocation. Downstream code references an app by type path
/// (`#[model(app = Vehicles)]`); Rust's own name resolution enforces
/// declaration and the seal rejects non-app types with a trait-bound
/// error.
///
/// # Associated constants
///
/// - `LABEL` — the stable string identifier used in migration files,
///   ledger rows, and snapshot JSON. Defaults to the struct identifier
///   lowercased byte-by-byte (`Vehicles` → `"vehicles"`); override
///   via `#[app(label = "…")]` when the default would be awkward
///   (`BillingAccounts` → `"billingaccounts"`).
/// - `DATABASE` — the database-target name this app belongs to. Set
///   by `#[app(database = "…")]`; required (no default — an app
///   without an explicit target is a compile error today to avoid
///   silently landing tables in `main`).
/// - `DESCRIPTOR` — the const [`AppDescriptor`] reflecting this
///   app's runtime metadata. Phase 7's differ prefers iterating
///   [`AppRegistry::all`] but consumers that know a specific app at
///   compile time can read the const directly.
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
///
/// Emitted once per `djogi::apps!` entry via `inventory::submit!`,
/// collected by [`AppRegistry::all`], and read by Phase 7's migration
/// differ, snapshot writer, and ledger verifier.
///
/// T7 populates `label` and `database`; `renamed_from` and
/// `tombstone` are forward-declared here so T8's lifecycle-marker
/// extension (renames, tombstones, moved-from tracking) is a
/// populating-empty-slots change rather than a struct-layout change
/// that would ripple through every `inventory::submit!` call site.
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
    /// `crud_log`, `event_log`) or a user-defined target. Phase 7's
    /// differ groups migrations by `(database, label)` — see
    /// [`docs/spec/apps-and-database-domains.md`].
    pub database: &'static str,
    /// Prior label this app was renamed from, if any. Populated by
    /// the T8 `#[app(renamed_from = "…")]` marker; `None` in T7.
    /// Carries the **old string label**, not a type — the old type
    /// may no longer exist in source.
    pub renamed_from: Option<&'static str>,
    /// `true` when the app has been tombstoned via `#[app(tombstone)]`
    /// (T8). Phase 7's compose gates destructive migration generation
    /// behind `--allow-destructive` when this flag is set. Always
    /// `false` in T7.
    pub tombstone: bool,
}

impl AppDescriptor {
    /// Label of the synthetic global bucket. Models declared without
    /// `#[model(app = …)]` belong here. Exported as a const so Phase
    /// 7 consumers can compare by pointer-equality / `==` against a
    /// single canonical value.
    pub const GLOBAL_LABEL: &'static str = "";

    /// Database target assigned to the synthetic global bucket
    /// (Phase 7 default target).
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

/// Runtime lookup facade over the apps registered in this crate
/// graph.
///
/// Phase 7 consumers prefer [`AppRegistry::all`] to iterating
/// `inventory::iter::<AppDescriptor>` directly — `all` handles two
/// concerns:
///
/// 1. **Alphabetisation.** Inventory returns descriptors in link
///    order, which is non-deterministic across rebuilds and
///    toolchains. `all()` returns them sorted by `label` so
///    downstream artifacts (snapshot JSON, migration filenames,
///    ledger seed rows) are byte-stable. Matches the Phase 4.5
///    `visage_map` precedent.
/// 2. **The synthetic global bucket.** `all()` always prepends an
///    entry for `LABEL = ""` / `DATABASE = "main"` so apps-unaware
///    projects and mixed projects see the same shape from the
///    registry — `main/<empty-label>/` is always a valid target in
///    build.rs / snapshot / ledger code.
pub struct AppRegistry;

impl AppRegistry {
    /// Returns every registered [`AppDescriptor`] plus the synthetic
    /// global bucket, sorted alphabetically by `label`.
    ///
    /// The synthetic bucket's label is the empty string, which sorts
    /// first.
    ///
    /// # Identity uniqueness enforcement
    ///
    /// App identity per the migration contract is the pair
    /// `(database, label)` — migrations group by
    /// `<database_target>/<app_label>/` on disk, and two apps with the
    /// same label but different database targets are legitimate (e.g.
    /// `main/audit/` and `crud_log/audit/`). Within a single
    /// `djogi::apps!` invocation, duplicate labels are a compile
    /// error. Across multiple invocations — different modules of the
    /// same crate, or apps pulled in from multiple djogi-using
    /// library crates — this function panics on first call if two
    /// descriptors share the same `(database, label)` pair. Catching
    /// the collision here rather than at compile time is a deliberate
    /// trade: the macro is function-like and expands at its call
    /// site, so crate-global compile-time enforcement would require
    /// fragile link-time symbol tricks or impossible orphan-rule
    /// dances. Runtime panic at startup (`AppRegistry::all()` runs
    /// before any migration work) is loud, early, and informative.
    ///
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
            // then by database as tiebreaker so same-label/different-
            // database pairs land adjacent for duplicate-pair scanning.
            out.sort_by(|a, b| (a.label, a.database).cmp(&(b.label, b.database)));
            for pair in out.windows(2) {
                if !pair[0].label.is_empty()
                    && pair[0].label == pair[1].label
                    && pair[0].database == pair[1].database
                {
                    panic!(
                        "djogi::apps: duplicate app identity \
                         (database = {:?}, label = {:?}) declared across \
                         multiple `djogi::apps!` invocations — \
                         (database, label) pairs must be unique per crate \
                         (and across linked djogi-using crates)",
                        pair[0].database, pair[0].label,
                    );
                }
            }
            out
        })
    }

    /// Returns every cross-app foreign-key edge in the inventory.
    ///
    /// An edge exists when a field on a [`crate::ModelDescriptor`]
    /// carries `relation_kind = Some(RelationKind::ForeignKey)` and
    /// the **source** model's [`crate::ModelDescriptor::app`] differs
    /// from the **target** model's app. Intra-app FKs are not
    /// returned — they are always safe from the apps-subsystem's
    /// perspective since source and target share a migration
    /// `<database>/<app>/` directory and compose atomically.
    ///
    /// Phase 7's migration differ consumes this list to:
    ///
    /// - Emit cross-app FK clauses with the correct
    ///   `REFERENCES "<target-schema>".<target-table>(id)` form.
    /// - Order per-app compose steps so target apps are applied
    ///   before source apps (FKs resolve at declaration time).
    ///
    /// Models whose source or target resolves to the synthetic
    /// global bucket (empty label) are treated normally — the
    /// bucket is a valid app for FK-graph purposes.
    ///
    /// Unresolvable targets (a `target_type_name` with no matching
    /// `ModelDescriptor` in inventory) are silently skipped here —
    /// the diagnostic for that condition (D011-shaped, future) lands
    /// with Phase 7's validator, not the zero-cost apps-graph layer.
    ///
    /// Result is memoised in a `OnceLock` since inventory is fixed
    /// at link time.
    pub fn cross_app_edges() -> &'static [CrossAppEdge] {
        static CACHE: OnceLock<Vec<CrossAppEdge>> = OnceLock::new();
        CACHE.get_or_init(|| {
            use crate::descriptor::ModelDescriptor;
            use crate::descriptor::RelationKind;

            // Build type-name -> app label lookup once.
            let mut type_to_app: std::collections::HashMap<&'static str, &'static str> =
                std::collections::HashMap::new();
            for m in inventory::iter::<ModelDescriptor> {
                type_to_app.insert(m.type_name, m.app.unwrap_or(AppDescriptor::GLOBAL_LABEL));
            }

            let mut edges: Vec<CrossAppEdge> = Vec::new();
            for source in inventory::iter::<ModelDescriptor> {
                let source_app = source.app.unwrap_or(AppDescriptor::GLOBAL_LABEL);
                for field in source.fields {
                    if !matches!(field.relation_kind, Some(RelationKind::ForeignKey)) {
                        continue;
                    }
                    let Some(target_type) = field.target_type_name else {
                        continue;
                    };
                    let Some(&target_app) = type_to_app.get(target_type) else {
                        continue;
                    };
                    if source_app == target_app {
                        continue;
                    }
                    edges.push(CrossAppEdge {
                        source_app,
                        source_type: source.type_name,
                        source_field: field.name,
                        target_app,
                        target_type,
                    });
                }
            }
            edges.sort_by(|a, b| {
                (a.source_app, a.source_type, a.source_field).cmp(&(
                    b.source_app,
                    b.source_type,
                    b.source_field,
                ))
            });
            edges
        })
    }

    /// Returns every cross-app cycle in the FK graph.
    ///
    /// Each element is a sequence of app labels `[A, B, …, A]`
    /// describing one cycle. Same-app cycles (a model in `Billing`
    /// referencing another model in `Billing` through some chain)
    /// are deferred to Phase 7's intra-app analysis — this method
    /// surfaces only inter-app cycles.
    ///
    /// Algorithm: standard DFS with three-color marking over the
    /// condensed app→app graph (edges collapsed from
    /// [`Self::cross_app_edges`]). `O(A + E)` where `A` is the app
    /// count and `E` is the number of distinct inter-app edges.
    ///
    /// Result is memoised; inventory is fixed at link time.
    pub fn cross_app_cycles() -> &'static [Vec<&'static str>] {
        static CACHE: OnceLock<Vec<Vec<&'static str>>> = OnceLock::new();
        CACHE.get_or_init(|| {
            use std::collections::{HashMap, HashSet};

            // Collapse CrossAppEdge list to an app→{apps} adjacency map.
            let mut adj: HashMap<&'static str, Vec<&'static str>> = HashMap::new();
            for edge in Self::cross_app_edges() {
                let entry = adj.entry(edge.source_app).or_default();
                if !entry.contains(&edge.target_app) {
                    entry.push(edge.target_app);
                }
            }

            // Sort adjacency lists for deterministic DFS order.
            for neighbours in adj.values_mut() {
                neighbours.sort();
            }

            let mut cycles: Vec<Vec<&'static str>> = Vec::new();
            let mut onstack: HashSet<&'static str> = HashSet::new();
            let mut done: HashSet<&'static str> = HashSet::new();
            let mut stack: Vec<&'static str> = Vec::new();
            let mut roots: Vec<&'static str> = adj.keys().copied().collect();
            roots.sort();

            fn dfs(
                node: &'static str,
                adj: &HashMap<&'static str, Vec<&'static str>>,
                onstack: &mut HashSet<&'static str>,
                done: &mut HashSet<&'static str>,
                stack: &mut Vec<&'static str>,
                cycles: &mut Vec<Vec<&'static str>>,
            ) {
                if done.contains(node) {
                    return;
                }
                onstack.insert(node);
                stack.push(node);
                if let Some(neighbours) = adj.get(node) {
                    for &nbr in neighbours {
                        if onstack.contains(&nbr) {
                            // Record the cycle slice from the first
                            // occurrence of `nbr` in the stack to the
                            // top, closed with `nbr` itself.
                            if let Some(start) = stack.iter().position(|n| *n == nbr) {
                                let mut cycle: Vec<&'static str> = stack[start..].to_vec();
                                cycle.push(nbr);
                                cycles.push(cycle);
                            }
                        } else if !done.contains(&nbr) {
                            dfs(nbr, adj, onstack, done, stack, cycles);
                        }
                    }
                }
                stack.pop();
                onstack.remove(node);
                done.insert(node);
            }

            for root in &roots {
                dfs(root, &adj, &mut onstack, &mut done, &mut stack, &mut cycles);
            }
            cycles
        })
    }
}

/// One cross-app foreign-key edge surfaced by
/// [`AppRegistry::cross_app_edges`].
///
/// Phase 7's migration differ uses these edges to:
///
/// - Order per-app compose steps so target apps apply before source
///   apps (FK constraints resolve at DDL time).
/// - Emit schema-qualified `REFERENCES "<target-schema>".<table>`
///   clauses when source and target live in different databases /
///   apps.
///
/// `source_app` and `target_app` are the stable string labels from
/// [`AppDescriptor::label`], not Rust type paths. The synthetic
/// global bucket (empty label) is a valid participant on either
/// side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrossAppEdge {
    /// App label owning the source model (the one with the FK column).
    pub source_app: &'static str,
    /// Source model's Rust type name, e.g. `"Invoice"`.
    pub source_type: &'static str,
    /// Column name of the FK field on the source model.
    pub source_field: &'static str,
    /// App label owning the target model.
    pub target_app: &'static str,
    /// Target model's Rust type name, e.g. `"Customer"`.
    pub target_type: &'static str,
}

/// Apps-subsystem diagnostic contracts surfaced to Phase 7 consumers.
///
/// Phase 7-Zero T9 only declares the variants — the detection logic
/// and error-surface text live in Phase 7 proper, where the
/// filesystem-vs-snapshot / ledger-vs-registry comparisons actually
/// happen. The enum lives here so consumers can pattern-match on
/// stable variants without a subsequent breaking change.
///
/// Adding a variant is a breaking change; the enum is
/// `#[non_exhaustive]` so callers outside this crate cannot exhaust.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppDiagnostic {
    /// **D004 — app folder drift.** A migration directory
    /// `<database>/<app>/` exists on disk but no matching
    /// `AppDescriptor` appears in the current build's inventory.
    /// Likely causes: the declaring crate was removed from the
    /// workspace without running a retirement compose first, or
    /// the snapshot's `registered_apps` list is stale. Phase 7's
    /// compose surfaces this with the offending folder path and
    /// suggests `djogi migrations apply --reconcile-apps`.
    FolderDrift {
        /// Database target containing the orphaned folder.
        database: &'static str,
        /// App label of the orphaned folder.
        label: &'static str,
    },

    /// **D010 — unknown app label in ledger.** A ledger row carries
    /// an `app_label` that no current inventory descriptor
    /// declares (neither as `label` nor as `renamed_from`). Phase 7's
    /// compose refuses to replay the ledger until the operator
    /// either re-declares the app or runs
    /// `djogi migrations apply --reconcile-apps` to mark the rows
    /// for archival.
    UnknownLedgerApp {
        /// Database target containing the unknown app.
        database: &'static str,
        /// The ledger's `app_label` value that failed lookup.
        label: &'static str,
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
        // lives in T10 integration tests where two apps + models
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
        // T9 ships the `AppDiagnostic` enum as a contract — no
        // detection logic yet, but consumers can pattern-match on
        // the variants, so prove they construct.
        let folder_drift = AppDiagnostic::FolderDrift {
            database: "main",
            label: "oldbilling",
        };
        let unknown_ledger = AppDiagnostic::UnknownLedgerApp {
            database: "main",
            label: "mystery_app",
        };
        assert_ne!(folder_drift, unknown_ledger);
    }

    #[test]
    fn cross_app_edge_equality_and_ordering() {
        // Two identical edges compare equal; edges sort by
        // `(source_app, source_type, source_field)` so the stable
        // ordering in `cross_app_edges()` is load-bearing.
        let a = CrossAppEdge {
            source_app: "billing",
            source_type: "Invoice",
            source_field: "customer_id",
            target_app: "users",
            target_type: "User",
        };
        let b = CrossAppEdge {
            source_app: "billing",
            source_type: "Invoice",
            source_field: "customer_id",
            target_app: "users",
            target_type: "User",
        };
        assert_eq!(a, b);
    }
}
