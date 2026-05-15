//! Query API — lazy `QuerySet<T>`, typed filters, SQL emission.
//!
//! The public surface is re-exported at crate root and in `prelude`:
//! users write `use djogi::prelude::*;` and get `QuerySet`, `FieldRef`,
//! `Lookup`, `Q`, etc. without a second import.
//!
//! Internally: `queryset` holds the builder state, `condition` the filter
//! tree (the legacy substrate, retired by Cluster 8γ T6.9), `q` the
//! public Q-algebra (the post-8γ substrate), `field` the typed column
//! handles, `order` ordering expressions, `filter` the programmatic-builder
//! types, `update` bulk-update assignments, `sql` the `ConditionBuilder` +
//! SQL emitters, and `terminal` the `fetch_*` methods. Splitting by
//! responsibility keeps each file auditable.
//!
//! # Public vs internal surface
//!
//! User code composes filters through `Q<T>` (the public algebra
//! introduced by Cluster 8γ) and `FieldRef` lookup methods — **never** by
//! constructing `Leaf`/`FilterValue`/`LookupOp` variants directly. The
//! raw AST types remain reachable under [`internal`] for in-tree
//! consumers (the SQL emitter, migration differ, shell bindings) and
//! for integration tests that assert on the tree shape, but they are
//! not peer public API with `Q` / `FieldRef`. Treat paths inside
//! `internal` as unstable — variant names, payload shapes, and the
//! module layout can shift across phases without a semver bump.
//!
//! # Substrate — Q<T> alongside Condition during the 8γ transition
//!
//! `Condition` is the pre-8γ filter tree; `Q<T>` is the post-8γ
//! public algebra. The Cluster 8γ refactor introduces `Q<T>` as an
//! additive surface first (T6.1–T6.5 + T6.10–T6.13) so adopters and
//! sister clusters (8β / 8δ / 8ε) can compose against the new shape
//! without waiting for the substrate swap. T6.6–T6.9 then retire the
//! internal `Condition` enum and route every `QuerySet<T>::filter`
//! through `Q<T>`. Both types remain reachable through this stage of
//! the work; `pub use condition::Condition` stays in place so existing
//! `FieldRef::eq` / `gt` / `ilike` etc. callers continue compiling.

pub mod aggregate;
pub mod annotate;
pub mod closure;
pub mod condition;
pub mod field;
pub mod filter;
pub mod grouped;
pub(crate) mod lock;
pub mod order;
// Phase 8eta PR2a — Djogi-owned portable predicate substrate.
//
// `predicate` is the public re-export module for `PortablePredicate<T>` and
// the (PR2b-populated) `Predicate<T>` shell. `portable` is hidden because
// PR2b's macro-emitted `Model::__djogi_emit_field_predicate` overrides have
// to name `SqlEmitContext` and `PortablePredicateError` from a path
// reachable cross-crate; `pub(crate)` would not survive cross-crate macro
// expansion, while `#[doc(hidden)] pub` plus the `__private::query::*` route
// in `lib.rs` does. See `query/portable.rs` for the rationale.
#[doc(hidden)]
pub mod portable;
pub mod predicate;
pub mod q;
pub mod queryset;
pub mod recursive;
pub(crate) mod refresh;
// Phase 8.5 Cluster 4B (#101) — typed set operations between same-model
// `QuerySet<T>` instances. The module is `pub` so adopters can name
// `SetOpQuerySet` / `SetOpKind` as parameter and return types; the
// common case (chained `.union(...)` / `.union_all(...)` /
// `.intersect(...)` / `.except(...)`) uses the builder methods emitted
// onto [`QuerySet`] and [`SetOpQuerySet`] themselves so most adopters
// never name the module path.
pub mod set_op;
#[cfg(feature = "spatial")]
pub mod spatial_grouping;
pub(crate) mod sql;
pub mod stream;
pub mod terminal;
pub mod update;
pub mod visage_queryset;

pub use aggregate::AggregateQuery;
pub use annotate::{AnnotatedQuerySet, IntoAggregateTuple};
pub use closure::{ClosureModel, MaterializeClosureOptions, MaterializeClosureReport};
// `Condition` is NOT re-exported at this level post-Cluster 8γ Stage 2.
// The public substrate is `Q<T>` (re-exported below); legacy
// `Condition`-producing FieldRef lookup methods (`f.col.eq(v)` etc.) are
// still in use by the closure API (`QuerySet::filter` / `exclude`), so
// the type itself stays reachable at `crate::query::condition::Condition`
// for inference. Removing it from the public re-export tree closes the
// "downstream Into<Condition> ambiguity" attack v3 §T6 Codex bullet
// calls out — adopter code that needs to name the type uses
// `crate::query::internal::Condition` (the unstable namespace below)
// or composes via `Q<T>` instead.
// Phase 8eta PR2a — Djogi root field wrapper surface.
//
// `DjogiField`, `DjogiPresentField`, `ExplicitPgPredicateField`, and the
// `DjogiPortableOrd` trait are introduced additively in PR2a so the
// public re-export tree compiles before macros and SQL emitters flip in
// PR2b/PR2c/PR2d. `FieldRef` / `IntoFilterValue` / `OptionalRelationRef`
// remain re-exported as before — generated `{Model}Fields` accessors still
// return `FieldRef` until PR3 flips the macro emission.
pub use condition::ConditionExt;
pub use field::{
    DjogiField, DjogiPortableOrd, DjogiPresentField, ExplicitPgPredicateField, FieldRef,
    IntoFieldFilterValue, IntoFilterValue, IntoPortableFieldValue, IntoSqlField,
    OptionalRelationRef,
};
pub use filter::{FilterClause, Lookup, ModelFilter};
pub use order::{Direction, NullsOrder, OrderExpr};
// Phase 8eta PR2a — public predicate-wrapper surface.
//
// `PortablePredicate<T>` is the trusted-Djogi-provenance wrapper that flows
// through PR2b's direct-`Q<T>` SQL emitter and PR4's cache boundary.
// `Predicate<T>` is the PR2b operator-matrix shell. `IntoPortablePredicate`
// is the sealed trait Djogi-owned root field methods consume (currently
// implemented only for `PortablePredicate<T>`).
pub use predicate::{IntoPortablePredicate, PortablePredicate, Predicate};
// Hidden re-exports for macro-emitted code and PR2b's direct-SQL walker.
// `SqlEmitContext` and `PortablePredicateError` appear in the public
// `Model::__djogi_emit_field_predicate` hook signature and in PR2d's
// generated overrides; routing them through `__private` (see `lib.rs`)
// keeps adopter code from naming them while letting macro-expanded
// `impl Model for {Model}` blocks compile.
#[doc(hidden)]
pub use portable::{PortablePredicateError, SqlEmitContext};
pub use q::{ArrayPredicate, IntoQ, Q};
pub use queryset::{
    CachedPortableQuerySet, DistinctMode, IntoDistinctColumns, PortableQuerySet, QuerySet,
};
pub use recursive::{RecursiveDirection, RecursiveQuerySet};
// Phase 8.5 Cluster 4B (#101) — typed set operations. `IntoSetOpArm` is
// the sealed trait the builder methods take; adopters never name it,
// but it must be re-exported so trait-method dispatch resolves in
// downstream crates. `SetOpKind` / `SetOpQuerySet` are named in adopter
// return / parameter types when threading a chained set-op through
// helper functions.
pub use set_op::{IntoSetOpArm, SetOpKind, SetOpQuerySet};
// `BasicPredicate<T>` is sassi's universal Rust-evaluable predicate algebra.
// Re-exported here so adopters reach it as `djogi::query::BasicPredicate`
// without depending on sassi directly. The Cluster 8γ refactor (T6) lifts
// the 15 Rust-evaluable `LookupOp` variants into `sassi::BasicPredicate`
// while keeping the 2 SQL-only ops (`Regex`, `IRegex`) on the djogi side
// (`Q::Regex`) — see spec §8e bullet 6 and `decisions.md` row 107 + 108.
pub use sassi::BasicPredicate;
#[cfg(feature = "spatial")]
pub use spatial_grouping::{ClusterId, ClusterRadius, GeohashKey, GeohashPrecision, RegionKey};
pub use stream::{ModelCursorStream, RawCursorStream};
pub use update::{IntoAssignments, UpdateAssignment, UpdateStmt};
pub use visage_queryset::VisageQuerySet;

/// Raw Condition-AST surface — not peer public API with `Condition`.
///
/// Holds `Leaf`, `FilterValue`, and `LookupOp` for framework-internal
/// consumers (SQL emitter, differ, shell). User code that finds itself
/// reaching for these is a sign the `FieldRef` API is missing a lookup
/// method — please file an issue rather than building leaves by hand.
///
/// # Stability
///
/// Items re-exported here follow the same variant-level `#[non_exhaustive]`
/// guarantees as `FilterValue` / `LookupOp` themselves, but the **set of
/// items** in this module, and its path, may change across phases. Pin
/// your own type aliases if you depend on them.
pub mod internal {
    // Cluster 8γ Stage 2 (T6.9b): `Condition` graduates from peer
    // public API (it was `pub use condition::Condition` at module
    // root pre-flip) into the unstable internal namespace alongside
    // `Leaf` / `FilterValue` / `LookupOp`. Cluster 8β's
    // `default_filter_condition() -> Option<Condition>` trait method
    // names this type and rebases against this path; future code
    // composing through the public algebra never needs to name it
    // (closure-side `FieldRef::eq` etc. type-infer the return).
    pub use super::condition::{Condition, FilterValue, Leaf, LookupOp};
}

#[cfg(test)]
mod tests {
    /// Compile-only sanity: `BasicPredicate<T>` is reachable both as
    /// `sassi::BasicPredicate` (the originating crate path) and as
    /// `crate::query::BasicPredicate` (the re-export adopters depend on).
    /// This locks the re-export contract: removing the `pub use` line
    /// would silently break adopter call-sites; this test catches it
    /// at the compilation step.
    #[test]
    fn basic_predicate_reachable_from_djogi_query() {
        let _: sassi::BasicPredicate<()> = sassi::BasicPredicate::True;
        let _: crate::query::BasicPredicate<()> = crate::query::BasicPredicate::True;
    }

    /// Phase 8eta PR2a — re-export contract for the Djogi predicate
    /// substrate. The `pub use predicate::{...}` and `pub use field::{...}`
    /// lines above must remain in place; PR2b's macro emission, PR4's
    /// cache boundary, and adopter `impl DjogiPortableOrd` callsites all
    /// reach these names through `crate::query::*`. Compile-only — every
    /// `use` line is the contract.
    #[test]
    fn phase8eta_pr2a_predicate_substrate_reachable_from_djogi_query() {
        #[allow(unused_imports)]
        use crate::query::{
            DjogiField, DjogiPortableOrd, DjogiPresentField, ExplicitPgPredicateField,
            IntoPortablePredicate, PortablePredicate, Predicate,
        };

        // `DjogiPortableOrd` reachable as a public trait. The bound check
        // is compile-only — PR2a's listed scalar impls are exercised by
        // this generic helper.
        fn assert_djogi_portable_ord<T: DjogiPortableOrd>() {}
        assert_djogi_portable_ord::<i64>();
        assert_djogi_portable_ord::<crate::HeerId>();
        assert_djogi_portable_ord::<rust_decimal::Decimal>();
    }

    /// Hidden re-exports for macro-emitted code: `SqlEmitContext` and
    /// `PortablePredicateError`. These are reached through
    /// `::djogi::__private::query` from generated `impl Model` blocks; this
    /// test covers the in-crate path.
    #[test]
    fn phase8eta_pr2a_hidden_emit_context_reachable() {
        let _: crate::query::SqlEmitContext = crate::query::SqlEmitContext::root();
        let _: crate::query::SqlEmitContext = crate::query::SqlEmitContext::joined("posts");
        // Compile-only: variant exists.
        let _ = crate::query::PortablePredicateError::UnsupportedModel { model: "Test" };
    }
}
