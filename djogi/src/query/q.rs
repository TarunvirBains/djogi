//! `Q<T>` — the public predicate algebra over model `T`.
//!
//! `Q<T>` is the substrate every `QuerySet<T>` accumulates after the
//! Cluster 8γ refactor. It composes through Rust's standard bitwise
//! operators (`&` / `|` / `^` / `!`, desugaring to AND / OR / XOR /
//! NOT) and lifts directly from `sassi::BasicPredicate<T>` for
//! Rust-evaluable predicates.
//!
//! # Design — variant set
//!
//! At skeleton stage (T6.2) `Q<T>` carries:
//!
//! | Variant            | Surface                            | Where it evaluates |
//! |--------------------|------------------------------------|--------------------|
//! | `Q::Basic(p)`      | `BasicPredicate<T>` from sassi     | Rust + SQL         |
//! | `Q::Ilike(f, s)`   | `col ILIKE $1`                     | SQL only           |
//! | `Q::JsonbPath(l)`  | `(col->'a')::cast op $1`           | SQL only           |
//! | `Q::Regex(...)`    | `col ~ $1` / `col ~* $1` (POSIX)   | SQL only           |
//! | `Q::Expression(e)` | `Expr<bool>` escape hatch          | SQL only           |
//! | `Q::Array(p)`      | `@>`, `<@`, `&&`                   | SQL only           |
//!
//! Operator overloads (`Q::Compound`, `Q::Xor`, `Q::Negated`) land at
//! T6.3 / T6.5; the lowering bridge to `Condition` lands at T6.6.
//!
//! ## FTS and spatial route through `Q::Expression`
//!
//! Spec §8e bullet 1 lists `Q::FullText` and `Q::Spatial` as named
//! variants. The shipped design subsumes both into `Q::Expression`
//! because:
//!
//! - `FtsFieldRef::matches(q) -> Condition` already produces
//!   `Condition::Expr(Expr::from_node(ExprNode::TsMatch { … }))`. There
//!   is no FTS-specific predicate type to wrap; the Phase 5 expression
//!   IR carries the full FTS payload (column, dictionary, query text).
//! - Every spatial predicate method (`within_km`, `intersects`,
//!   `covers`, `bounded_by`, `dwithin_km`) returns
//!   `Condition::Expr(Expr::from_node(ExprNode::Spatial(SpatialExpr::…)))`.
//!   Same observation: the typed wrapper would carry the same payload
//!   as `Expr<bool>` without adding any compile-time guarantees.
//!
//! Adding stub `Q::FullText` / `Q::Spatial` variants whose only job is
//! to carry an `Expr<bool>` would split one escape hatch across three
//! variants without buying type safety. The lens (six axes per
//! `feedback_decision_priorities.md`) lands cleanly here: idiomatic
//! Rust + simple-to-use both prefer the smaller variant set, and no
//! axis pulls the other direction (scalability / completeness /
//! security / production-stability are all neutral on the choice).
//!
//! If a future refactor surfaces a typed FTS / spatial wrapper that
//! captures information the expression IR does not (e.g. typed
//! coordinate-system metadata for spatial), the variants can be added
//! at that point — `Q<T>` is `#[non_exhaustive]`, so adding variants
//! is non-breaking.
//!
//! # The §660 split — Rust-evaluable vs SQL-only
//!
//! Per spec §8e bullet 6 (`docs/spec/implementation-plan.md:660`): the
//! 15 Rust-evaluable lookup operators (`Eq`, `Neq`, `Gt`, `Gte`, `Lt`,
//! `Lte`, `In`, `NotIn`, `IsNull`, `IsNotNull`, `Between`, `IContains`,
//! `IStartsWith`, `IEndsWith`, `IExact`) lift to
//! `sassi::BasicPredicate::Field` and ride through `Q::Basic`. The 2
//! SQL-only operators (`Regex`, `IRegex` — Postgres POSIX `~` / `~*`)
//! stay djogi-side as `Q::Regex(field, pattern, case_sensitive)`.
//!
//! The split is load-bearing per `decisions.md` rows 107 + 108 and the
//! `feedback_no_regex_in_djogi.md` memory anchor. Lifting `Regex` /
//! `IRegex` to `BasicPredicate` would require a Rust regex engine,
//! which the framework forbids. Trybuild fixture
//! `phase8_lookup_op_regex_lifted_to_basic_predicate.rs` (T6.10) locks
//! the rule at the type level.

use crate::model::Model;
use crate::query::field::FieldRef;
use sassi::BasicPredicate;

/// Public Q-algebra over model `T`. Wraps `sassi::BasicPredicate<T>`
/// for Rust-evaluable predicates plus djogi-only SQL extensions
/// (ILIKE, JSONB path, Postgres POSIX regex, expression IR escape
/// hatch, array operators). `Q::Expression` is the single escape
/// hatch for any `Expr<bool>` — that includes FTS via
/// `ExprNode::TsMatch` and spatial via `ExprNode::Spatial(...)` (see
/// module docs for why FTS and spatial are not separate variants).
///
/// Marked `#[non_exhaustive]` — adding new SQL-only variants must not
/// break downstream pattern matches.
///
/// `T: Model` is the same bound as `FieldRef<T, V>` and `QuerySet<T>`
/// — every variant either constructs a column ref or routes through
/// the model's typed surface. `BasicPredicate<T>` itself does not
/// require `T: Model` (it works on plain structs in sassi proper),
/// but any meaningful djogi-side use of `Q::Basic` lifts the bound
/// because the model carries the column-name registry the SQL
/// emitter needs at lowering time.
///
/// At T6.2 the enum is skeleton-only — no `BitAnd` / `BitOr` /
/// `BitXor` / `Not` impls and no internal `Compound` / `Xor` /
/// `Negated` nodes. Those land at T6.3 (operators on pure-Basic
/// operands) and T6.5 (mixed-operand internal nodes).
#[derive(Debug, Clone)]
#[non_exhaustive]
#[allow(dead_code)] // T6.2 ships the skeleton; consumers wire up at T6.3+.
pub enum Q<T: Model> {
    /// Rust-evaluable predicates lifted from sassi.
    /// `BasicPredicate::True` / `False` cover the vacuous identities
    /// — `Q<T>` does not duplicate them as separate variants.
    Basic(BasicPredicate<T>),

    /// `col ILIKE $1` — case-insensitive LIKE. SQL-only because LIKE
    /// pattern semantics (`%`, `_`, `\\` escape) are not reproducible
    /// in Rust without a regex engine, which djogi forbids
    /// (`decisions.md` row 107).
    Ilike(FieldRef<T, String>, String),

    /// JSONB-path leaf — wraps the existing
    /// [`crate::jsonb::path::JsonbPathLeaf`] from Phase 5.
    JsonbPath(crate::jsonb::path::JsonbPathLeaf),

    /// Postgres POSIX regex — `col ~ $1` (case-sensitive when the
    /// flag is `true`, `col ~* $1` when `false`).
    ///
    /// **SQL-only** per `decisions.md` row 108. The match runs
    /// server-side; no Rust regex engine is linked. Lifting this
    /// variant to `Q::Basic(BasicPredicate::Field(_))` would require
    /// a Rust regex engine and is forbidden — see `decisions.md`
    /// row 107 and `feedback_no_regex_in_djogi.md`. Trybuild fixture
    /// `phase8_lookup_op_regex_lifted_to_basic_predicate.rs` (T6.10)
    /// locks the rule at the type level.
    Regex(FieldRef<T, String>, String, /* case_sensitive */ bool),

    /// Escape hatch for typed-expression predicates (Phase 4 Task
    /// 3a). Subsumes FTS (`ExprNode::TsMatch`) and spatial
    /// (`ExprNode::Spatial(SpatialExpr::…)`) — see module docs for
    /// the design choice.
    Expression(crate::expr::Expr<bool>),

    /// Array operators — `@>`, `<@`, `&&` over Postgres array
    /// columns. The leaf shape lands at T6.4; this commit ships the
    /// stub.
    Array(ArrayPredicate<T>),
}

/// Array-column predicates — wraps the existing array leaves
/// produced by `FieldRef<M, Vec<V>>::contains` / `contained_by` /
/// `overlap`.
///
/// `PhantomData<T>` keeps `ArrayPredicate<T>` covariant in the model
/// type so it slots cleanly into `Q<T>` without affecting variance
/// elsewhere. The leaves themselves carry typed bind values
/// (`Vec<V>` flattened through `FilterValue::Array*`) and do not
/// reference `T`; the phantom marker exists purely for the algebra's
/// per-model parameterization.
///
/// `#[non_exhaustive]` so future array operators (`array_length`,
/// `cardinality`, custom GIN/GiST indexable ops) can be added
/// without breaking downstream pattern matches.
///
/// At T6.2 this is the skeleton shape; T6.4 adds `From` impls
/// converting from raw leaves into `ArrayPredicate<T>`, and from
/// `ArrayPredicate<T>` into `Q<T>` via a generic blanket.
#[derive(Debug, Clone)]
#[non_exhaustive]
#[allow(dead_code)] // T6.2 ships the skeleton; T6.4 adds the From impls.
pub enum ArrayPredicate<T: Model> {
    /// `col @> $1` — array contains.
    Contains(crate::array::ArrayContainsLeaf, std::marker::PhantomData<T>),
    /// `col <@ $1` — array contained by.
    ContainedBy(
        crate::array::ArrayContainedByLeaf,
        std::marker::PhantomData<T>,
    ),
    /// `col && $1` — array overlap.
    Overlap(crate::array::ArrayOverlapLeaf, std::marker::PhantomData<T>),
}

#[cfg(test)]
#[allow(clippy::manual_async_fn)]
// The `Model` trait's CRUD methods return `impl Future + Send` rather than
// using `async fn` syntax (pinned to Send explicitly). The inert test stub
// below mirrors that trait shape, which trips `clippy::manual_async_fn` under
// Rust 1.93+. Allow the lint on this module only — rewriting the trait
// itself is out of scope for the algebra refactor.
mod tests {
    use super::*;
    use crate::DjogiError;
    use crate::descriptor::ModelDescriptor;

    // Minimal test-model. Same shape as the in-crate test models used
    // by `query::field::tests` — every async hook is `unreachable!()`
    // because the algebra-level tests never invoke them. The empty
    // `Fields = ()` tuple is enough; `Q::Basic` / `Q::Ilike` etc. only
    // need `M: Model`, not a populated field accessor surface.
    //
    // `#[derive(Clone)]` on the marker is required because
    // `BasicPredicate<T>` derives `Clone` (which propagates a `T: Clone`
    // bound) and `Q<T>` therefore picks up the same bound. Real models
    // typically derive `Clone` already; the bound only matters for
    // these marker-only test types.
    #[derive(Clone, Debug)]
    struct TestModel;

    impl crate::model::__sealed::Sealed for TestModel {}
    impl Model for TestModel {
        type Pk = crate::types::HeerId;
        type Fields = ();
        fn table_name() -> &'static str {
            "test_models"
        }
        fn pk_value(&self) -> &Self::Pk {
            unreachable!("algebra tests do not invoke pk_value")
        }
        fn descriptor() -> &'static ModelDescriptor {
            unreachable!("algebra tests do not invoke descriptor")
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: Self::Pk,
        ) -> impl std::future::Future<Output = Result<Self, DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), DjogiError>> + Send + 'ctx {
            async { unreachable!() }
        }
        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<Self, DjogiError>> + Send + 'ctx {
            async { unreachable!() }
        }
    }

    /// `Q::Basic` constructs from a sassi `BasicPredicate` — the
    /// load-bearing path for the §660 split.
    #[test]
    fn q_skeleton_constructs_basic_variant() {
        let q: Q<TestModel> = Q::Basic(BasicPredicate::True);
        assert!(matches!(q, Q::Basic(BasicPredicate::True)));
    }

    /// `Q::Basic(BasicPredicate::False)` is the vacuous-falsehood
    /// identity. Locks the contract that `Q<T>` does not duplicate
    /// True/False as separate top-level variants.
    #[test]
    fn q_skeleton_carries_basic_false() {
        let q: Q<TestModel> = Q::Basic(BasicPredicate::False);
        assert!(matches!(q, Q::Basic(BasicPredicate::False)));
    }

    /// `Clone` and `Debug` derives sanity check — these are required
    /// by the `Q::Compound { parts: Vec<Q<T>> }` shape that lands at
    /// T6.5 (Vec needs Clone for cheap composition).
    #[test]
    fn q_skeleton_is_clone_and_debug() {
        let q: Q<TestModel> = Q::Basic(BasicPredicate::True);
        let _ = format!("{:?}", q.clone());
    }
}
