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
}
