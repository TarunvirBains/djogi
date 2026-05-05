//! `Q<T>` — the public predicate algebra over model `T`.
//!
//! `Q<T>` is the substrate adopters compose to filter `QuerySet<T>`.
//! It composes through Rust's standard bitwise operators (`&` / `|`
//! / `^` / `!`, desugaring to AND / OR / XOR / NOT) and lifts
//! directly from `sassi::BasicPredicate<T>` for Rust-evaluable
//! predicates.
//!
//! # Variant grammar
//!
//! | Variant            | Surface                            | Where it evaluates |
//! |--------------------|------------------------------------|--------------------|
//! | `Q::Basic(p)`      | `BasicPredicate<T>` from sassi     | Rust + SQL         |
//! | `Q::Ilike(f, s)`   | `col ILIKE $1`                     | SQL only           |
//! | `Q::JsonbPath(l)`  | `(col->'a')::cast op $1`           | SQL only           |
//! | `Q::Regex(...)`    | `col ~ $1` / `col ~* $1` (POSIX)   | SQL only           |
//! | `Q::Expression(e)` | `Expr<bool>` escape hatch          | SQL only           |
//! | `Q::Array(p)`      | `@>`, `<@`, `&&`                   | SQL only           |
//! | `Q::Compound`      | `AND` / `OR` over mixed siblings   | both               |
//! | `Q::Xor(a, b)`     | XOR (general form `(¬a∧b)∨(a∧¬b)`) | both               |
//! | `Q::Negated(q)`    | `NOT (...)`                        | both               |
//!
//! `Q<T>` is `#[non_exhaustive]` — adding new SQL-only variants is
//! non-breaking; downstream pattern matches must include `_ => …`.
//!
//! # Operator precedence
//!
//! Rust's precedence table: `&` > `^` > `|` (AND tighter than XOR
//! tighter than OR). So
//!
//! ```ignore
//! Q::Basic(...) ^ Q::Ilike(...) | Q::Negated(...)
//! ```
//!
//! parses as `(Basic ^ Ilike) | Negated`. The trybuild compile-pass
//! `phase8_q_algebra_xor_precedence.rs` (T6.11) locks the parse at
//! the type level; runtime tests
//! `q_operator_precedence_*` in `query::q::tests` lock the resulting
//! `Q::Compound` / `Q::Xor` shape.
//!
//! # Internal compound nodes — when the substrate uses what
//!
//! Pure-Basic compositions short-circuit through sassi's flattening
//! reducer. `Q::from(a) & Q::from(b)` produces a single
//! `Q::Basic(BasicPredicate::And(vec![a, b]))` rather than wrapping
//! in `Q::Compound`. Mixed-operand compositions (at least one side
//! is not `Q::Basic`) lift to:
//!
//! - `Q::Compound { op: And | Or, parts: Vec<Q<T>> }` for the
//!   associative operators (And/Or). Flattens on construction:
//!   `(a & b) & c` produces a 3-element `parts` Vec rather than a
//!   nested binary tree.
//! - `Q::Xor(Box<Q<T>>, Box<Q<T>>)` for XOR — non-associative, so
//!   flattening would silently re-associate. Mirrors sassi's
//!   `BasicPredicate::Xor(Box, Box)` shape.
//! - `Q::Negated(Box<Q<T>>)` for NOT over non-Basic operands.
//!   Pure-Basic negation rides sassi's `Not` (which collapses
//!   double-negation in place); mixed wraps. `!Q::Negated(inner)`
//!   collapses to `*inner` to avoid stacked `NOT NOT` SQL.
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
use std::ops::{BitAnd, BitOr, BitXor, Not};

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
/// # Internal compound nodes
///
/// Pure-Basic compositions short-circuit through sassi's flattening
/// reducer, so `Q::from(a) & Q::from(b)` produces a single
/// `Q::Basic(BasicPredicate::And(vec![a, b]))` rather than an outer
/// `Q::Compound`. Mixed-operand compositions (at least one side is
/// not `Q::Basic`) lift to `Q::Compound { op, parts }` for And / Or
/// (which are associative and flatten cleanly), to `Q::Xor(a, b)`
/// for XOR (non-associative — must stay binary), and to
/// `Q::Negated(inner)` for Not over non-Basic operands. Pure-Basic
/// negation rides sassi's `Not` (which collapses double-negation in
/// place); only the mixed side needs the new variant.
#[derive(Debug, Clone)]
#[non_exhaustive]
#[allow(dead_code)] // Variants populated as T6.6/T6.7 wire consumers; constructors land at T6.4.
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
    /// columns.
    Array(ArrayPredicate<T>),

    /// Mixed-operand And/Or — at least one side is not pure
    /// `Q::Basic`. Pure-Basic And/Or short-circuits through
    /// `Q::Basic(BasicPredicate::And(_))` to keep flattening +
    /// evaluation centralised in sassi. Flattens on construction:
    /// `(a & b) & c` produces `Q::Compound { op: And, parts: [a, b, c] }`
    /// rather than a nested binary tree. An empty `Vec` is the vacuous
    /// identity (And empty = TRUE, Or empty = FALSE) but combinator
    /// construction never produces one.
    Compound { op: CompoundOp, parts: Vec<Q<T>> },

    /// SQL XOR over two Q-algebra terms. Non-associative — `(a ^ b)
    /// ^ c ≠ a ^ (b ^ c)` in general, so XOR cannot ride a
    /// flattened `parts: Vec<_>` like And/Or do. Mirrors sassi's
    /// `BasicPredicate::Xor(Box, Box)` shape. The general-form SQL
    /// emit is `(NOT a AND b) OR (a AND NOT b)`; a boolean fast-path
    /// (`a <> b` when both operands are pre-evaluated booleans) is
    /// deferred to T11 per v3 §T6 deliverables bullet 3.
    Xor(Box<Q<T>>, Box<Q<T>>),

    /// SQL `NOT (...)` over a non-Basic Q. Pure-Basic negation rides
    /// sassi's `Not` (which collapses double-negation in place); this
    /// variant only exists for the SQL-only side. `!Q::Negated(inner)`
    /// collapses to `*inner` to avoid stacked `NOT NOT` nodes.
    Negated(Box<Q<T>>),
}

/// Operator marker for `Q::Compound`. Restricted to the associative
/// operators (And / Or) — XOR is non-associative and lives in the
/// dedicated `Q::Xor(Box, Box)` variant. Adding a new associative
/// operator (e.g. a sassi-side n-ary reducer) is non-breaking via
/// `#[non_exhaustive]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompoundOp {
    /// SQL `(a AND b AND c)`. Empty parts vector is the vacuous-truth
    /// identity (matches `Condition::And(empty) == TRUE` from
    /// `condition.rs:24`).
    And,
    /// SQL `(a OR b OR c)`. Empty parts vector is the vacuous-falsehood
    /// identity (matches `Condition::Or(empty) == FALSE`).
    Or,
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
#[derive(Debug, Clone)]
#[non_exhaustive]
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

// ── `From` impls — array leaves lift into `ArrayPredicate<T>` then `Q<T>` ────
//
// Three explicit impls per leaf rather than a generic blanket. A blanket
// `impl<T: Model, L> From<L> for Q<T> where ArrayPredicate<T>: From<L>` would
// be the smallest amount of code, but it surfaces type-inference surprises
// at adopter callsites — particularly anywhere a numeric literal or string
// could otherwise satisfy `From<L>`. Three explicit impls lock the lift
// path one leaf type at a time and keep `cargo expand` output legible.

impl<T: Model> From<crate::array::ArrayContainsLeaf> for ArrayPredicate<T> {
    fn from(leaf: crate::array::ArrayContainsLeaf) -> Self {
        ArrayPredicate::Contains(leaf, std::marker::PhantomData)
    }
}

impl<T: Model> From<crate::array::ArrayContainedByLeaf> for ArrayPredicate<T> {
    fn from(leaf: crate::array::ArrayContainedByLeaf) -> Self {
        ArrayPredicate::ContainedBy(leaf, std::marker::PhantomData)
    }
}

impl<T: Model> From<crate::array::ArrayOverlapLeaf> for ArrayPredicate<T> {
    fn from(leaf: crate::array::ArrayOverlapLeaf) -> Self {
        ArrayPredicate::Overlap(leaf, std::marker::PhantomData)
    }
}

impl<T: Model> From<ArrayPredicate<T>> for Q<T> {
    fn from(p: ArrayPredicate<T>) -> Self {
        Q::Array(p)
    }
}

impl<T: Model> From<crate::array::ArrayContainsLeaf> for Q<T> {
    fn from(leaf: crate::array::ArrayContainsLeaf) -> Self {
        Q::Array(leaf.into())
    }
}

impl<T: Model> From<crate::array::ArrayContainedByLeaf> for Q<T> {
    fn from(leaf: crate::array::ArrayContainedByLeaf) -> Self {
        Q::Array(leaf.into())
    }
}

impl<T: Model> From<crate::array::ArrayOverlapLeaf> for Q<T> {
    fn from(leaf: crate::array::ArrayOverlapLeaf) -> Self {
        Q::Array(leaf.into())
    }
}

// ── `Q<T>` constructors + operator overloads ─────────────────────────────────

impl<T: Model> Q<T> {
    /// Vacuous-truth identity — `Q::Basic(BasicPredicate::True)`.
    /// Helper for reductions; matches the existing `Condition::True`
    /// idiom and is used by `QuerySet::default()` once T6.9 swaps the
    /// substrate.
    #[must_use]
    pub fn always_true() -> Self {
        Q::Basic(BasicPredicate::True)
    }

    /// Vacuous-falsehood identity — `Q::Basic(BasicPredicate::False)`.
    #[must_use]
    pub fn always_false() -> Self {
        Q::Basic(BasicPredicate::False)
    }
}

impl<T: Model> From<BasicPredicate<T>> for Q<T> {
    /// Lift a sassi `BasicPredicate<T>` into the djogi `Q<T>` algebra
    /// without duplicating the reducer. `From` is the canonical
    /// idiomatic conversion; `.into()` at adopter callsites picks
    /// this up automatically — `let q: Q<V> = my_basic.into();`.
    fn from(p: BasicPredicate<T>) -> Self {
        Q::Basic(p)
    }
}

// `BitAnd` / `BitOr` / `BitXor` / `Not` impls — std already marks the
// trait methods `#[must_use]` (their result is the only meaningful
// product of the call), so re-adding the attribute on the impl
// methods is redundant and lints under rustc 1.95+. The `Q::*`
// constructors and `IntoQ::into_q` carry the `#[must_use]` that
// matters at adopter callsites.

impl<T: Model> BitAnd for Q<T> {
    type Output = Q<T>;
    /// SQL AND. Pure-Basic operands flatten through sassi's
    /// `BasicPredicate::bitand` so chained `a & b & c` produces a
    /// single `Q::Basic(BasicPredicate::And(vec![a, b, c]))` — keeping
    /// the flattened-And invariant centralised in sassi. Mixed
    /// operands lift to `Q::Compound { op: And, parts }` and flatten
    /// when either side is already a Compound-And.
    fn bitand(self, rhs: Self) -> Q<T> {
        compose_compound(self, rhs, CompoundOp::And, BasicPredicate::bitand)
    }
}

impl<T: Model> BitOr for Q<T> {
    type Output = Q<T>;
    /// SQL OR. Dual of `BitAnd::bitand` — pure-Basic flattens through
    /// sassi; mixed operands lift to `Q::Compound { op: Or, parts }`.
    fn bitor(self, rhs: Self) -> Q<T> {
        compose_compound(self, rhs, CompoundOp::Or, BasicPredicate::bitor)
    }
}

impl<T: Model> BitXor for Q<T> {
    type Output = Q<T>;
    /// SQL XOR. Pure-Basic operands ride sassi's
    /// `BasicPredicate::bitxor` (which itself produces
    /// `BasicPredicate::Xor(Box, Box)`). Mixed operands lift to
    /// `Q::Xor(Box, Box)` directly. **Non-associative** — XOR
    /// chains do NOT flatten into a `Vec<_>` (unlike And/Or), so
    /// `(a ^ b) ^ c` is a left-leaning binary tree, not a 3-element
    /// flat node. Mirrors sassi's choice (`BasicPredicate::Xor` is
    /// also binary).
    ///
    /// **Operator precedence reminder.** Rust binds `&` tighter than
    /// `^`, and `^` tighter than `|`. So
    /// `Q::from(a) ^ Q::Ilike(...) | Q::Expression(...)` parses as
    /// `(Q::from(a) ^ Q::Ilike(...)) | Q::Expression(...)`. T6.11
    /// trybuild compile-pass locks this at the type level.
    fn bitxor(self, rhs: Self) -> Q<T> {
        match (self, rhs) {
            (Q::Basic(a), Q::Basic(b)) => Q::Basic(a ^ b),
            (lhs, rhs) => Q::Xor(Box::new(lhs), Box::new(rhs)),
        }
    }
}

impl<T: Model> Not for Q<T> {
    type Output = Q<T>;
    /// SQL `NOT (...)`. Pure-Basic operands ride sassi's `Not`,
    /// which collapses double-negation in place (`!!p == p`) and
    /// flips `True` ↔ `False`. Mixed operands wrap in
    /// `Q::Negated(...)`; `!Q::Negated(inner)` collapses to `*inner`
    /// to avoid stacked `NOT NOT` nodes that would emit redundant
    /// SQL parens. De Morgan's transformation is **not** applied
    /// across `Q::Compound` — the SQL emitter renders
    /// `Q::Negated(Q::Compound{...})` as `NOT (...)` directly,
    /// matching the existing `Condition::Not(Condition::And(...))`
    /// behavior.
    fn not(self) -> Q<T> {
        match self {
            Q::Basic(p) => Q::Basic(!p),
            Q::Negated(inner) => *inner,
            other => Q::Negated(Box::new(other)),
        }
    }
}

/// And/Or composition shared between `BitAnd` and `BitOr`. Pure-Basic
/// operands delegate to sassi's flattening reducer (passed in as
/// `basic_op`); mixed operands lift to `Q::Compound { op, parts }`
/// with the same flattening contract sassi uses internally:
/// `(Compound{op, parts: l}, Compound{op, parts: r})` extends `l`
/// with `r`, `(Compound{op, parts: l}, other)` pushes onto `l`,
/// `(other, Compound{op, parts: r})` prepends, and the bare-binary
/// case wraps `vec![lhs, rhs]`.
///
/// The `op` parameter is the Compound marker for the mixed path; the
/// `basic_op` parameter is the sassi delegate for the pure-Basic
/// path. Splitting the two avoids a `match (lhs, rhs, op)` tower at
/// each callsite while keeping the shared shape DRY.
fn compose_compound<T: Model, F>(lhs: Q<T>, rhs: Q<T>, op: CompoundOp, basic_op: F) -> Q<T>
where
    F: FnOnce(BasicPredicate<T>, BasicPredicate<T>) -> BasicPredicate<T>,
{
    match (lhs, rhs) {
        (Q::Basic(a), Q::Basic(b)) => Q::Basic(basic_op(a, b)),
        (
            Q::Compound {
                op: lop,
                parts: mut l,
            },
            Q::Compound { op: rop, parts: r },
        ) if lop == op && rop == op => {
            l.extend(r);
            Q::Compound { op, parts: l }
        }
        (
            Q::Compound {
                op: lop,
                parts: mut l,
            },
            other,
        ) if lop == op => {
            l.push(other);
            Q::Compound { op, parts: l }
        }
        (other, Q::Compound { op: rop, parts: r }) if rop == op => {
            let mut v = vec![other];
            v.extend(r);
            Q::Compound { op, parts: v }
        }
        (lhs, rhs) => Q::Compound {
            op,
            parts: vec![lhs, rhs],
        },
    }
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

    /// Convenience constructor — sassi's `True` lifts to `Q::Basic`.
    #[test]
    fn q_always_true_is_basic_true() {
        let q: Q<TestModel> = Q::always_true();
        assert!(matches!(q, Q::Basic(BasicPredicate::True)));
    }

    /// Dual: `always_false` is `Q::Basic(False)`.
    #[test]
    fn q_always_false_is_basic_false() {
        let q: Q<TestModel> = Q::always_false();
        assert!(matches!(q, Q::Basic(BasicPredicate::False)));
    }

    /// `From<BasicPredicate<T>>` route — the `.into()` adopter idiom.
    #[test]
    fn q_from_basic_predicate_via_into() {
        let bp: BasicPredicate<TestModel> = BasicPredicate::True;
        let q: Q<TestModel> = bp.into();
        assert!(matches!(q, Q::Basic(BasicPredicate::True)));
    }

    /// Pure-Basic AND short-circuits through sassi — the resulting
    /// `Q::Basic` carries a flattened `BasicPredicate::And(_)`.
    #[test]
    fn q_basic_and_basic_flattens_through_sassi() {
        let a: Q<TestModel> = BasicPredicate::True.into();
        let b: Q<TestModel> = BasicPredicate::False.into();
        match a & b {
            Q::Basic(BasicPredicate::And(parts)) => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(parts[0], BasicPredicate::True));
                assert!(matches!(parts[1], BasicPredicate::False));
            }
            other => panic!("expected Q::Basic(BasicPredicate::And(_)), got {other:?}"),
        }
    }

    /// Pure-Basic OR short-circuits through sassi as well.
    #[test]
    fn q_basic_or_basic_flattens_through_sassi() {
        let a: Q<TestModel> = BasicPredicate::True.into();
        let b: Q<TestModel> = BasicPredicate::False.into();
        assert!(matches!(a | b, Q::Basic(BasicPredicate::Or(_))));
    }

    /// Pure-Basic XOR rides sassi's `BasicPredicate::Xor(Box, Box)`.
    /// No flattening — XOR is non-associative.
    #[test]
    fn q_basic_xor_basic_uses_sassi_xor() {
        let a: Q<TestModel> = BasicPredicate::True.into();
        let b: Q<TestModel> = BasicPredicate::False.into();
        assert!(matches!(a ^ b, Q::Basic(BasicPredicate::Xor(_, _))));
    }

    /// Pure-Basic double-negation collapses through sassi's `Not`
    /// reducer (`!!p == p`). The outer `Q::Basic` wraps the
    /// already-collapsed sassi result; no `Q::Negated` should appear.
    #[test]
    fn q_basic_double_negation_collapses_via_sassi() {
        let p: Q<TestModel> = BasicPredicate::True.into();
        let result = !!p;
        assert!(matches!(result, Q::Basic(BasicPredicate::True)));
    }

    /// Mixed-operand AND lifts to `Q::Compound`.
    #[test]
    fn q_mixed_and_creates_compound_node() {
        let a: Q<TestModel> = BasicPredicate::True.into();
        let b: Q<TestModel> = Q::Negated(Box::new(BasicPredicate::False.into()));
        match a & b {
            Q::Compound {
                op: CompoundOp::And,
                parts,
            } => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(parts[0], Q::Basic(_)));
                assert!(matches!(parts[1], Q::Negated(_)));
            }
            other => panic!("expected Q::Compound{{op: And, ..}}, got {other:?}"),
        }
    }

    /// Chained mixed-AND flattens — `(a & b) & c` produces a
    /// 3-element `parts: Vec<_>`, not a 2-element Vec with a nested
    /// inner `Q::Compound`.
    #[test]
    fn q_chained_compound_and_flattens() {
        // Force mixed by wrapping at least one operand in `Q::Negated`
        // (which prevents the pure-Basic short-circuit).
        let neg = || Q::<TestModel>::Negated(Box::new(BasicPredicate::True.into()));
        let combined = neg() & neg() & neg();
        match combined {
            Q::Compound {
                op: CompoundOp::And,
                parts,
            } => assert_eq!(parts.len(), 3, "expected flat 3-element parts"),
            other => panic!("expected Q::Compound{{op: And, ..}}, got {other:?}"),
        }
    }

    /// Chained mixed-OR flattens identically to AND.
    #[test]
    fn q_or_with_two_compounds_flattens() {
        let neg = || Q::<TestModel>::Negated(Box::new(BasicPredicate::True.into()));
        let lhs = neg() | neg();
        let rhs = neg() | neg();
        match lhs | rhs {
            Q::Compound {
                op: CompoundOp::Or,
                parts,
            } => assert_eq!(parts.len(), 4),
            other => panic!("expected Q::Compound{{op: Or, ..}}, got {other:?}"),
        }
    }

    /// Mixed-XOR lands in `Q::Xor(Box, Box)` directly. Locks the
    /// non-associativity decision: no flattening allowed.
    #[test]
    fn q_xor_mixed_lands_in_q_xor_variant() {
        let basic: Q<TestModel> = BasicPredicate::True.into();
        let neg: Q<TestModel> = Q::Negated(Box::new(BasicPredicate::False.into()));
        match basic ^ neg {
            Q::Xor(lhs, rhs) => {
                assert!(matches!(*lhs, Q::Basic(_)));
                assert!(matches!(*rhs, Q::Negated(_)));
            }
            other => panic!("expected Q::Xor(_, _), got {other:?}"),
        }
    }

    /// `Q::Negated(Q::Negated(inner))` collapses to `inner` on the
    /// `Not::not` path. Required so `!!q` doesn't pile up SQL `NOT
    /// NOT (...)` nesting in the eventual emitter output.
    #[test]
    fn q_not_negated_negated_collapses() {
        // Wrap something non-Basic so the negation lands in `Q::Negated`
        // rather than collapsing through sassi.
        let inner: Q<TestModel> = Q::Compound {
            op: CompoundOp::And,
            parts: vec![
                Q::Negated(Box::new(BasicPredicate::True.into())),
                Q::Negated(Box::new(BasicPredicate::False.into())),
            ],
        };
        let result = !!inner.clone();
        // After two `Not::not` calls: first wraps in `Q::Negated`, second
        // unwraps. Result is the original non-Basic inner.
        assert!(matches!(result, Q::Compound { .. }));
    }

    /// Operator precedence runtime check. Locks Rust's table:
    /// `&` > `^` > `|`. So `Q::from(a) ^ Q::Negated(...) | Q::Negated(...)`
    /// parses as `(Q::from(a) ^ Q::Negated(...)) | Q::Negated(...)`.
    /// Trybuild compile-pass at T6.11 doubles this with a
    /// type-level lock; this runtime test validates the resulting
    /// `Q` shape.
    #[test]
    fn q_operator_precedence_xor_binds_tighter_than_or() {
        let a: Q<TestModel> = BasicPredicate::True.into();
        let b: Q<TestModel> = Q::Negated(Box::new(BasicPredicate::False.into()));
        let c: Q<TestModel> = Q::Negated(Box::new(BasicPredicate::True.into()));

        let composed = a ^ b | c;

        // Outer should be Or (lowest-precedence binding).
        match composed {
            Q::Compound {
                op: CompoundOp::Or,
                parts,
            } => {
                assert_eq!(parts.len(), 2);
                // Left half: the XOR result.
                assert!(matches!(parts[0], Q::Xor(_, _)));
                // Right half: Q::Negated.
                assert!(matches!(parts[1], Q::Negated(_)));
            }
            other => panic!("expected outer Q::Compound{{op: Or}}, got {other:?}"),
        }
    }

    /// `ArrayContainsLeaf` lifts to `ArrayPredicate::Contains` via
    /// `From`, then to `Q::Array(...)` via the secondary lift. Locks
    /// the chain so adopters can write `Q::from(field.contains(&[1, 2]))`
    /// without naming the intermediate type.
    #[test]
    fn q_array_contains_lifts_via_into() {
        use crate::array::ArrayContainsLeaf;
        use crate::query::condition::FilterValue;
        let leaf = ArrayContainsLeaf {
            column: "tags",
            values: FilterValue::ArrayString(vec!["a".to_string()]),
        };
        let q: Q<TestModel> = leaf.into();
        match q {
            Q::Array(ArrayPredicate::Contains(inner, _)) => {
                assert_eq!(inner.column, "tags");
            }
            other => panic!("expected Q::Array(ArrayPredicate::Contains), got {other:?}"),
        }
    }

    /// `ArrayContainedByLeaf` lifts identically.
    #[test]
    fn q_array_contained_by_lifts_via_into() {
        use crate::array::ArrayContainedByLeaf;
        use crate::query::condition::FilterValue;
        let leaf = ArrayContainedByLeaf {
            column: "tags",
            values: FilterValue::ArrayI32(vec![1, 2, 3]),
        };
        let q: Q<TestModel> = leaf.into();
        assert!(matches!(q, Q::Array(ArrayPredicate::ContainedBy(_, _))));
    }

    /// `ArrayOverlapLeaf` lifts identically.
    #[test]
    fn q_array_overlap_lifts_via_into() {
        use crate::array::ArrayOverlapLeaf;
        use crate::query::condition::FilterValue;
        let leaf = ArrayOverlapLeaf {
            column: "tags",
            values: FilterValue::ArrayBool(vec![true]),
        };
        let q: Q<TestModel> = leaf.into();
        assert!(matches!(q, Q::Array(ArrayPredicate::Overlap(_, _))));
    }

    /// Exhaustive match over `ArrayPredicate<TestModel>` covers all
    /// three variants today. Locks the variant set against accidental
    /// drift; new variants added under `#[non_exhaustive]` will need
    /// to extend this match (and the SQL emitter at T6.6/T6.9).
    #[test]
    fn q_array_three_variants_exhaust() {
        use crate::array::{ArrayContainedByLeaf, ArrayContainsLeaf, ArrayOverlapLeaf};
        use crate::query::condition::FilterValue;

        // `#[non_exhaustive]` doesn't apply to in-crate matches, so
        // this exhaustive match compiles. Cross-crate code must still
        // include a `_ => …` arm.
        let leaves: [ArrayPredicate<TestModel>; 3] = [
            ArrayContainsLeaf {
                column: "tags",
                values: FilterValue::ArrayI32(vec![1]),
            }
            .into(),
            ArrayContainedByLeaf {
                column: "tags",
                values: FilterValue::ArrayI32(vec![1]),
            }
            .into(),
            ArrayOverlapLeaf {
                column: "tags",
                values: FilterValue::ArrayI32(vec![1]),
            }
            .into(),
        ];
        for p in leaves {
            match p {
                ArrayPredicate::Contains(_, _) => {}
                ArrayPredicate::ContainedBy(_, _) => {}
                ArrayPredicate::Overlap(_, _) => {}
            }
        }
    }

    /// AND binds tighter than XOR. `a & b ^ c` parses as
    /// `(a & b) ^ c`, mirroring Rust's bit-operator precedence.
    #[test]
    fn q_operator_precedence_and_binds_tighter_than_xor() {
        let a: Q<TestModel> = BasicPredicate::True.into();
        let b: Q<TestModel> = Q::Negated(Box::new(BasicPredicate::False.into()));
        let c: Q<TestModel> = Q::Negated(Box::new(BasicPredicate::True.into()));

        let composed = a & b ^ c;

        // Outer Xor.
        match composed {
            Q::Xor(lhs, rhs) => {
                // Left half: the AND result. With one Basic and one
                // Negated, mixed-AND lifts to Q::Compound{op: And, ..}.
                assert!(matches!(
                    *lhs,
                    Q::Compound {
                        op: CompoundOp::And,
                        ..
                    }
                ));
                assert!(matches!(*rhs, Q::Negated(_)));
            }
            other => panic!("expected outer Q::Xor, got {other:?}"),
        }
    }
}
