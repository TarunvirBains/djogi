//! Query API — lazy `QuerySet<T>`, typed filters, SQL emission.
//! The public surface is re-exported at crate root and in `prelude`:
//! users write `use djogi::prelude::*;` and get `QuerySet`, `FieldRef`,
//! `Lookup`, `Q`, etc. without a second import.
//! Internally: `queryset` holds the builder state, `condition` the filter
//! tree (the legacy substrate, retired in favour of `Q<T>`), `q` the
//! public Q-algebra, `field` the typed column
//! handles, `order` ordering expressions, `filter` the programmatic-builder
//! types, `update` bulk-update assignments, `sql` the `ConditionBuilder` +
//! SQL emitters, and `terminal` the `fetch_*` methods. Splitting by
//! responsibility keeps each file auditable.
//! # Public vs internal surface
//! User code composes filters through `Q<T>` (the public algebra
//! `Q<T>`) and `FieldRef` lookup methods — **never** by
//! constructing `Leaf`/`FilterValue`/`LookupOp` variants directly. The
//! raw AST types remain reachable under [`internal`] for in-tree
//! consumers (the SQL emitter, migration differ, shell bindings) and
//! for integration tests that assert on the tree shape, but they are
//! not peer public API with `Q` / `FieldRef`. Treat paths inside
//! `internal` as unstable — variant names, payload shapes, and the
//! module layout can shift across phases without a semver bump.
//! # Substrate — Q<T> alongside Condition
//! `Condition` is the legacy filter tree; `Q<T>` is the
//! public algebra. The refactor introduces `Q<T>` as an
//! additive surface first so adopters can compose against the new shape
//! while the substrate swap completes. The internal `Condition` enum
//! is retired and every `QuerySet<T>::filter` routes through `Q<T>`.
//! Both types remain reachable; `pub use condition::Condition` stays in
//! place so existing
//! `FieldRef::eq` / `gt` / `ilike` etc. callers continue compiling.

pub mod aggregate;
pub mod annotate;
pub mod closure;
pub mod condition;
pub mod field;
pub mod filter;
pub mod grouped;
// Djogi#106) — typed bulk-copy surface
// `INSERT INTO target (cols...) SELECT exprs... FROM source [WHERE ...]`.
// Closes the framework gap that previously forced adopters to fall back
// to `ctx.raw_execute(...)` for cross-table archival / migration shapes.
pub mod insert_select;
// Typed pair-tuple query surface
// (`JoinedQuerySet<L, R>` + `JoinedAnnotatedQuerySet<L, R, A>`).
// Substrate for GH #99 (which is itself the substrate for #84). This
// slice covers part of the typed pair-tuple surface; the mating-pairs
// retrofit + full Punnu showcase land in follow-on tasks. See
// `query::joined` for the design rationale, SQL emission shape, and
// entry points.
pub mod joined;
pub mod lateral;
pub(crate) mod lock;
// Issue #178 — typed `MERGE INTO ... USING` query surface.
pub mod merge;
// Djogi#195 — `MirJzSON` JSON predicate builders.
// Wraps Sassi's `JSahibONFieldRef` / `JSahibONOptionFieldRef` /
// `JSahibONPathRef` / `JSahibONValueRef` surfaces with Djogi
// trusted-provenance stamping. Adopter code reaches this through
// `DjogiField<M, MirJzSON>::jsahibon()` (re-exported via the impl
// blocks in the module itself); the module path stays `pub` so the
// builder types are nameable when adopter code declares helper
// functions that consume a `DjogiJSahibONFieldRef<M>`.
pub mod mirjzson;
pub mod order;
// Djogi-owned portable predicate substrate.
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
// Row-shape aggregate terminals (#92)
// (`as_mvt(...)` / `as_geobuf(...)`). Gated on `feature = "spatial"`
// because both shipped row aggregates are PostGIS surfaces.
#[cfg(feature = "spatial")]
pub mod row_aggregate_terminal;
// #101) — typed set operations between same-model
// `QuerySet<T>` instances. The module is `pub` so adopters can name
// `SetOpQuerySet` / `SetOpKind` as parameter and return types; the
// common case (chained `.union(...)` / `.union_all(...)` /
// `.intersect(...)` / `.except(...)`) uses the builder methods emitted
// onto [`QuerySet`] and [`SetOpQuerySet`] themselves so most adopters
// never name the module path.
pub mod set_op;
#[cfg(feature = "spatial")]
pub mod spatial_grouping;
// Djogi#180 — PG18 OLD/NEW RETURNING result type.
// `ReturningPair<T>` is the public before/after snapshot for UPDATE returning.
// No PG17 fallback; Djogi already has a hard PG18 floor.
pub mod returning;
pub(crate) mod sql;
pub mod stream;
pub mod terminal;
pub mod update;
// Djogi#103 — typed VALUES inline-relation join surface.
pub mod values;
pub mod visage_queryset;

pub use aggregate::AggregateQuery;
pub use annotate::{AnnotatedQuerySet, IntoAggregateTuple};
pub use closure::{ClosureModel, MaterializeClosureOptions, MaterializeClosureReport};
// `Condition` is NOT re-exported at this level.
// The public substrate is `Q<T>` (re-exported below); legacy
// `Condition`-producing FieldRef lookup methods (`f.col.eq(v)` etc.) are
// still in use by the closure API (`QuerySet::filter` / `exclude`), so
// the type itself stays reachable at `crate::query::condition::Condition`
// for inference. Removing it from the public re-export tree closes the
// "downstream Into<Condition> ambiguity" attack
// calls out — adopter code that needs to name the type uses
// `crate::query::internal::Condition` (the unstable namespace below)
// or composes via `Q<T>` instead.
// Djogi root field wrapper surface.
// `DjogiField`, `DjogiPresentField`, `ExplicitPgPredicateField`, and the
// portable marker traits are introduced additively in PR2a so the
// public re-export tree compiles before macros and SQL emitters flip in
// PR2b/PR2c/PR2d. `FieldRef` / `IntoFilterValue` / `OptionalRelationRef`
// remain re-exported as before — generated `{Model}Fields` accessors still
// return `FieldRef` until PR3 flips the macro emission.
pub use condition::ConditionExt;
pub use field::{
    DjogiField, DjogiPortableEq, DjogiPortableOrd, DjogiPresentField, ExplicitPgOrderable,
    ExplicitPgPredicateField, FieldRef, IntoFieldFilterValue, IntoFilterValue,
    IntoPortableFieldValue, IntoSqlField, OptionalRelationRef,
};
// Djogi#195 — MirJzSON JSON predicate builder re-exports.
pub use filter::{FilterClause, Lookup, ModelFilter};
pub use mirjzson::{
    DjogiJSahibONFieldRef, DjogiJSahibONOptionFieldRef, DjogiJSahibONPathRef,
    DjogiJSahibONValueRef, MirJzSONFieldRef, MirJzSONOptionFieldRef,
};
// Djogi#106) — public re-export for the
// INSERT...SELECT surface. `InsertSelectColumn<S, T>` is the typed
// (target_column, source_expression) leaf carrying both source and
// target model identity at the type level; `InsertSelectSource<S, V>`
// is the source-tagged operand `copy_from` accepts;
// `IntoInsertColumns<S, T>` is the closure-return shape (single
// mapping OR `Vec` of mappings); and `InsertSelectStmt<S, T>` is the
// inert terminal-pending statement returned by `QuerySet::insert_into`.
pub use insert_select::{
    InsertSelectColumn, InsertSelectSource, InsertSelectStmt, IntoInsertColumns,
};
// Typed pair-tuple query surface re-exports.
pub use joined::{
    JoinedAnnotatedQuerySet, JoinedAnnotatedRow, JoinedQuerySet, PairClosureKinshipSum,
    PairOrderExpr, PairSide, PairWindowExt,
};
// Issue #178 — typed MERGE re-exports.
pub use lateral::{InnerLateral, LateralQuerySet, LeftLateral};
pub use merge::{
    IntoMergeInsertColumns, IntoMergeOn, IntoMergeTargetExpr, IntoMergeUpdates,
    IntoMergeWhenCondition, MergeAction, MergeBranch, MergeCounts, MergeInsertColumn,
    MergeMatchKind, MergeOnEq, MergeStmt, MergeTargetExpr, MergeUpdateAssignment,
    MergeWhenCondition,
};
// `PairAreaOverlapRatio<L, R>` ships only with the `spatial` feature
// flag enabled: its constructor's `SpatialColumnValue` bound and the
// `crate::geo::*` types its SQL emitter references are themselves
// `#[cfg(feature = "spatial")]`. Re-exporting it unconditionally would
// break no-default-features builds where the symbol does not exist.
#[cfg(feature = "spatial")]
pub use joined::PairAreaOverlapRatio;
pub use order::{Direction, NullsOrder, OrderExpr};
// Public predicate-wrapper surface.
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
// Row-shape aggregate terminals (#92)
// (`as_mvt(...)` / `as_geobuf(...)`). The terminals own their typed
// `Vec<u8>` decode and consume an annotation tuple; the `EmptyAnnotation`
// sentinel covers the no-annotation case so plain `QuerySet::as_mvt`
// produces an `AsMvtTerminal<T, EmptyAnnotation>`.
#[cfg(feature = "spatial")]
pub use row_aggregate_terminal::{AsGeobufTerminal, AsMvtTerminal, EmptyAnnotation};
// #101) — typed set operations. `IntoSetOpArm` is
// the sealed trait the builder methods take; adopters never name it,
// but it must be re-exported so trait-method dispatch resolves in
// downstream crates. `SetOpKind` / `SetOpQuerySet` are named in adopter
// return / parameter types when threading a chained set-op through
// helper functions.
pub use set_op::{IntoSetOpArm, SetOpKind, SetOpQuerySet};
// `BasicPredicate<T>` is sassi's universal Rust-evaluable predicate algebra.
// Re-exported here so adopters reach it as `djogi::query::BasicPredicate`
// without depending on sassi directly. The 15 Rust-evaluable
// `LookupOp` variants lift into `sassi::BasicPredicate`
// while keeping the 2 SQL-only ops (`Regex`, `IRegex`) on the djogi side
// (`Q::Regex`) — see spec §8e bullet 6 and `decisions.md` row 107 + 108.
pub use sassi::BasicPredicate;
#[cfg(feature = "spatial")]
pub use spatial_grouping::{ClusterId, ClusterRadius, GeohashKey, GeohashPrecision, RegionKey};
pub use stream::{ModelCursorStream, RawCursorStream};
// Djogi#180 — PG18 OLD/NEW RETURNING result type.
pub use returning::ReturningPair;
pub use update::{IntoAssignments, UpdateAssignment, UpdateStmt};
pub use visage_queryset::{VisageColumn, VisageExists, VisageQuerySet, VisageSubquery};
// Djogi#103 — typed VALUES join surface.
// `InlineValues`, the three queryset types, and the supporting traits are the
// user-facing names. `ValuesScalar` / `ValuesRow` / `IntoValuesColumns` are
// sealed but must be re-exported so trait-method dispatch (`eq_values`,
// `col0`, …) resolves in downstream crates. `ValuesFields` and
// `ValuesFieldRef` must be nameable as closure parameter / return types when
// adopters write helpers that accept an `ON` predicate directly.
// `CrossValuesJoinedQuerySet` (GH #299) is the unconditional Cartesian join.
pub use values::{
    CrossValuesJoinedQuerySet, InlineValues, IntoValuesColumns, LeftValuesJoinedQuerySet,
    ValuesFieldRef, ValuesFields, ValuesJoinedQuerySet, ValuesOn, ValuesRow, ValuesScalar,
};
/// Raw Condition-AST surface — not peer public API with `Condition`.
/// Holds `Leaf`, `FilterValue`, and `LookupOp` for framework-internal
/// consumers (SQL emitter, differ, shell). User code that finds itself
/// reaching for these is a sign the `FieldRef` API is missing a lookup
/// method — please file an issue rather than building leaves by hand.
/// # Stability
/// Items re-exported here follow the same variant-level `#[non_exhaustive]`
/// guarantees as `FilterValue` / `LookupOp` themselves, but the **set of
/// items** in this module, and its path, may change across phases. Pin
/// your own type aliases if you depend on them.
pub mod internal {
    // `Condition` graduates from peer
    // public API (it was `pub use condition::Condition` at module
    // root pre-flip) into the unstable internal namespace alongside
    // `Leaf` / `FilterValue` / `LookupOp`. The
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

    /// Re-export contract for the Djogi predicate
    /// substrate. The `pub use predicate::{...}` and `pub use field::{...}`
    /// lines above must remain in place; PR2b's macro emission, PR4's
    /// cache boundary, and adopter `impl DjogiPortableOrd` callsites all
    /// reach these names through `crate::query::*`. Compile-only — every
    /// `use` line is the contract.
    #[test]
    fn pr2a_predicate_substrate_reachable_from_djogi_query() {
        #[allow(unused_imports)]
        use crate::query::{
            DjogiField, DjogiPortableEq, DjogiPortableOrd, DjogiPresentField,
            ExplicitPgPredicateField, IntoPortablePredicate, PortablePredicate, Predicate,
        };

        // `DjogiPortableOrd` / `DjogiPortableEq` reachable as public traits. The bound check
        // is compile-only — PR2a's listed scalar impls are exercised by
        // this generic helper.
        fn assert_djogi_portable_ord<T: DjogiPortableOrd>() {}
        fn assert_djogi_portable_eq<T: DjogiPortableEq>() {}
        assert_djogi_portable_ord::<i64>();
        assert_djogi_portable_ord::<crate::HeerId>();
        assert_djogi_portable_ord::<rust_decimal::Decimal>();
        assert_djogi_portable_eq::<i64>();
        assert_djogi_portable_eq::<crate::HeerId>();
        assert_djogi_portable_eq::<rust_decimal::Decimal>();
    }

    /// Hidden re-exports for macro-emitted code: `SqlEmitContext` and
    /// `PortablePredicateError`. These are reached through
    /// `::djogi::__private::query` from generated `impl Model` blocks; this
    /// test covers the in-crate path.
    #[test]
    fn pr2a_hidden_emit_context_reachable() {
        let _: crate::query::SqlEmitContext = crate::query::SqlEmitContext::root();
        let _: crate::query::SqlEmitContext = crate::query::SqlEmitContext::joined("posts");
        // Compile-only: variant exists.
        let _ = crate::query::PortablePredicateError::UnsupportedModel { model: "Test" };
    }
}
