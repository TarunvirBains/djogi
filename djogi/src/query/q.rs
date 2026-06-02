//! `Q<T>` — the public predicate algebra over model `T`.
//! `Q<T>` is the substrate adopters compose to filter `QuerySet<T>`.
//! It composes through Rust's standard bitwise operators (`&` / `|`
//! / `^` / `!`, desugaring to AND / OR / XOR / NOT) and carries
//! Djogi-trusted [`PortablePredicate<T>`] leaves for Rust-evaluable
//! predicates.
//! # Variant grammar
//! | Variant | Surface | Where it evaluates |
//! |--------------------|------------------------------------|--------------------|
//! | `Q::Portable(p)` | Djogi `PortablePredicate<T>` | Rust + SQL |
//! | `Q::Ilike(f, s)` | `col ILIKE $1` | SQL only |
//! | `Q::JsonbPath(l)` | `(col->'a')::cast op $1` | SQL only |
//! | `Q::Regex(...)` | `col ~ $1` / `col ~* $1` (POSIX) | SQL only |
//! | `Q::Expression(e)` | `Expr<bool>` escape hatch | SQL only |
//! | `Q::Array(p)` | `@>`, `<@`, `&&` | SQL only |
//! | `Q::Condition(c)` | legacy [`Condition`] escape hatch | SQL only |
//! | `Q::Compound` | `AND` / `OR` over mixed siblings | both |
//! | `Q::Xor(a, b)` | XOR (general form `(¬a∧b)∨(a∧¬b)`) | both |
//! | `Q::Negated(q)` | `NOT (...)` | both |
//! `Q<T>` is `#[non_exhaustive]` — adding new SQL-only variants is
//! non-breaking; downstream pattern matches must include `_ => …`.
//! # Operator precedence
//! Rust's precedence table: `&` > `^` > `|` (AND tighter than XOR
//! tighter than OR). So
//! ```ignore
//! Q::Portable(...) ^ Q::Ilike(...) | Q::Negated(...)
//! ```
//! parses as `(Portable ^ Ilike) | Negated`. The lihaaf compile-pass
//! fixture `q_algebra_xor_precedence.rs` locks the parse
//! at the type level; runtime tests
//! `q_operator_precedence_*` in `query::q::tests` lock the resulting
//! `Q::Compound` / `Q::Xor` shape.
//! # Internal compound nodes — when the substrate uses what
//! Pure-Portable compositions short-circuit through Sassi's flattening
//! reducer (via the trusted [`PortablePredicate`] wrapper).
//! `Q::Portable(a) & Q::Portable(b)` produces a single
//! `Q::Portable(PortablePredicate::And(vec![a, b]))` rather than wrapping
//! in `Q::Compound`. Mixed-operand compositions (at least one side is
//! not `Q::Portable`) lift to:
//! - `Q::Compound { op: And | Or, parts: Vec<Q<T>> }` for the
//!   associative operators (And/Or). Flattens on construction:
//!   `(a & b) & c` produces a 3-element `parts` Vec rather than a
//!   nested binary tree.
//! - `Q::Xor(Box<Q<T>>, Box<Q<T>>)` for XOR — non-associative, so
//!   flattening would silently re-associate. Mirrors Sassi's
//!   `BasicPredicate::Xor(Box, Box)` shape.
//! - `Q::Negated(Box<Q<T>>)` for NOT over non-Portable operands.
//!   Pure-Portable negation rides Sassi's `Not` (which collapses
//!   double-negation in place); mixed wraps. `!Q::Negated(inner)`
//!   collapses to `*inner` to avoid stacked `NOT NOT` SQL.
//! ## FTS and spatial route through `Q::Expression`
//! Spec §8e bullet 1 lists `Q::FullText` and `Q::Spatial` as named
//! variants. The shipped design subsumes both into `Q::Expression`
//! because:
//! - `FtsFieldRef::matches(q) -> Condition` already produces
//!   `Condition::Expr(Expr::from_node(ExprNode::TsMatch { … }))`. There
//!   is no FTS-specific predicate type to wrap; the expression
//!   IR carries the full FTS payload (column, dictionary, query text).
//! - Every spatial predicate method (`within_km`, `intersects`,
//!   `covers`, `bounded_by`, `dwithin_km`) returns
//!   `Condition::Expr(Expr::from_node(ExprNode::Spatial(SpatialExpr::…)))`.
//!   Same observation: the typed wrapper would carry the same payload
//!   as `Expr<bool>` without adding any compile-time guarantees.
//!   Adding stub `Q::FullText` / `Q::Spatial` variants whose only job is
//!   to carry an `Expr<bool>` would split one escape hatch across three
//!   variants without buying type safety. The lens (six axes per
//!   `feedback_decision_priorities.md`) lands cleanly here: idiomatic
//!   Rust + simple-to-use both prefer the smaller variant set, and no
//!   axis pulls the other direction (scalability / completeness /
//!   security / production-stability are all neutral on the choice).
//!   If a future refactor surfaces a typed FTS / spatial wrapper that
//!   captures information the expression IR does not (e.g. typed
//!   coordinate-system metadata for spatial), the variants can be added
//!   at that point — `Q<T>` is `#[non_exhaustive]`, so adding variants
//!   is non-breaking.
//! # The §660 split — Rust-evaluable vs SQL-only
//! Per spec §8e bullet 6 (`docs/spec/implementation-plan.md:660`): the
//! 15 Rust-evaluable lookup operators (`Eq`, `Neq`, `Gt`, `Gte`, `Lt`,
//! `Lte`, `In`, `NotIn`, `IsNull`, `IsNotNull`, `Between`, `IContains`,
//! `IStartsWith`, `IEndsWith`, `IExact`) lift to
//! Djogi's [`PortablePredicate<T>`] (which wraps a trusted
//! `sassi::BasicPredicate::Field` underneath) and ride through
//! `Q::Portable`. The 2 SQL-only operators (`Regex`, `IRegex` — Postgres
//! POSIX `~` / `~*`) stay djogi-side as
//! `Q::Regex(field, pattern, case_sensitive)`.
//! The split is load-bearing per `decisions.md` rows 107 + 108 and the
//! `feedback_no_regex_in_djogi.md` memory anchor. Lifting `Regex` /
//! `IRegex` to `BasicPredicate` would require a Rust regex engine,
//! which the framework forbids. Lihaaf compile-fail fixture
//! `lookup_op_regex_lifted_to_basic_predicate.rs` locks
//! the rule at the type level.
//! # Portable provenance
//! `Q::Portable(PortablePredicate<T>)` carries a [`crate::query::field::DjogiFieldProvenance`]
//! marker constructible only by Djogi-owned `DjogiField` /
//! `DjogiPresentField` predicate methods. This ensures trusted provenance:
//! raw Sassi predicates can pair forged column names with unrelated extractor
//! closures (`Field::new("col_a", |x| &x.col_b)`), which would let SQL
//! emission target a different column from in-memory Punnu evaluation. The
//! [`PortablePredicate`] wrapper prevents this at the type level.

use crate::model::Model;
use crate::query::condition::{Condition, FilterValue, Leaf, LookupOp};
use crate::query::field::FieldRef;
use crate::query::predicate::PortablePredicate;
use sassi::BasicPredicate;
use std::ops::{BitAnd, BitOr, BitXor, Not};

/// Public Q-algebra over model `T`. Wraps Djogi-trusted
/// [`PortablePredicate<T>`] for Rust-evaluable predicates plus djogi-only
/// SQL extensions (ILIKE, JSONB path, Postgres POSIX regex, expression
/// IR escape hatch, array operators). `Q::Expression` is the single
/// escape hatch for any `Expr<bool>` — that includes FTS via
/// `ExprNode::TsMatch` and spatial via `ExprNode::Spatial(...)` (see
/// module docs for why FTS and spatial are not separate variants).
/// Marked `#[non_exhaustive]` — adding new SQL-only variants must not
/// break downstream pattern matches.
/// `T: Model` is the same bound as `FieldRef<T, V>` and `QuerySet<T>`
/// every variant either constructs a column ref or routes through
/// the model's typed surface.
/// # Internal compound nodes
/// Pure-Portable compositions short-circuit through Sassi's flattening
/// reducer (via the trusted [`PortablePredicate`] wrapper), so
/// `Q::Portable(a) & Q::Portable(b)` produces a single
/// `Q::Portable(PortablePredicate::And(...))` rather than an outer
/// `Q::Compound`. Mixed-operand compositions (at least one side is not
/// `Q::Portable`) lift to `Q::Compound { op, parts }` for And / Or (which
/// are associative and flatten cleanly), to `Q::Xor(a, b)` for XOR
/// (non-associative — must stay binary), and to `Q::Negated(inner)` for
/// Not over non-Portable operands. Pure-Portable negation rides Sassi's
/// `Not` (which collapses double-negation in place); only the mixed
/// side needs the new variant.
#[must_use = "Q<T> describes a filter predicate; use it in a QuerySet::filter_struct call or it has no effect"]
#[non_exhaustive]
pub enum Q<T: Model> {
    /// Rust-evaluable predicates carried in a trusted-provenance Djogi
    /// wrapper around `sassi::BasicPredicate<T>`. The wrapper's
    /// [`crate::query::field::DjogiFieldProvenance`] marker is
    /// constructible only by Djogi-owned root field methods, so SQL
    /// emission and Punnu cache evaluation cannot diverge through forged
    /// column / extractor pairs. `PortablePredicate::True` / `False`
    /// cover the vacuous identities — `Q<T>` does not duplicate them as
    /// separate variants.
    Portable(PortablePredicate<T>),

    /// `col ILIKE $1` — case-insensitive LIKE. SQL-only because LIKE
    /// pattern semantics (`%`, `_`, `\\` escape) are not reproducible
    /// in Rust without a regex engine, which djogi forbids
    /// (`decisions.md` row 107).
    Ilike(FieldRef<T, String>, String),

    /// JSONB-path leaf — wraps the existing
    /// [`crate::jsonb::path::JsonbPathLeaf`] from .
    JsonbPath(crate::jsonb::path::JsonbPathLeaf),

    /// Postgres POSIX regex — `col ~ $1` (case-sensitive when the
    /// flag is `true`, `col ~* $1` when `false`).
    /// **SQL-only** per `decisions.md` row 108. The match runs
    /// server-side; no Rust regex engine is linked. Lifting this
    /// variant to `Q::Portable(BasicPredicate::Field(_))` would require
    /// a Rust regex engine and is forbidden — see `decisions.md`
    /// row 107 and `feedback_no_regex_in_djogi.md`. Lihaaf compile-fail
    /// fixture `lookup_op_regex_lifted_to_basic_predicate.rs`
    /// locks the rule at the type level.
    Regex(FieldRef<T, String>, String, /* case_sensitive */ bool),

    /// Escape hatch for typed-expression predicates. Subsumes FTS
    /// (`ExprNode::TsMatch`) and spatial (`ExprNode::Spatial(SpatialExpr::…)`)
    /// see module docs for the design choice.
    Expression(crate::expr::Expr<bool>),

    /// Array operators — `@>`, `<@`, `&&` over Postgres array
    /// columns.
    Array(ArrayPredicate<T>),

    /// SQL-side escape hatch carrying a legacy [`Condition`] tree.
    /// Every callsite that assigns a `Condition` to
    /// `QuerySet<T>::condition` lifts the value through this variant.
    /// The lowering bridge ([`q_to_condition`]) unwraps it as the
    /// identity, so the SQL emitter sees the same `Condition` tree the
    /// legacy closure-based path produced — character-for-character SQL
    /// parity is preserved by construction.
    /// Adopters do not normally construct this variant by hand. It
    /// exists so:
    /// - The legacy [`QuerySet::filter`] / [`QuerySet::exclude`]
    ///   closure API (which returns `Condition` from `FieldRef::eq` /
    ///   `gt` / `ilike` / etc.) keeps compiling unchanged.
    /// - The [`crate::query::filter::ModelFilter`] programmatic
    ///   builder uses this variant for clauses that cannot be safely
    ///   reconstructed as portable Q leaves.
    /// - Other subsystems (`default_filter_condition`, etc.) that
    ///   still produce `Condition` can compose with `Q<T>` without a
    ///   parallel rewrite.
    ///   The variant is `pub` for cross-crate macro emission but is
    ///   effectively an implementation detail. Code that constructs it
    ///   directly is signalling "I have a typed-leaf path that the
    ///   public `Q<T>` algebra doesn't yet cover" — the long-term
    ///   answer is to extend the public algebra rather than reach
    ///   through here.
    ///   [`QuerySet::filter`]: crate::query::QuerySet::filter
    ///   [`QuerySet::exclude`]: crate::query::QuerySet::exclude
    Condition(Condition),

    /// Mixed-operand And/Or — at least one side is not pure
    /// `Q::Portable`. Pure-Portable And/Or short-circuits through
    /// `Q::Portable(PortablePredicate::And(_))` to keep flattening +
    /// evaluation centralised in Sassi. Flattens on construction:
    /// `(a & b) & c` produces `Q::Compound { op: And, parts: [a, b, c] }`
    /// rather than a nested binary tree. An empty `Vec` is the vacuous
    /// identity (And empty = TRUE, Or empty = FALSE) but combinator
    /// construction never produces one.
    Compound { op: CompoundOp, parts: Vec<Q<T>> },

    /// SQL XOR over two Q-algebra terms. Non-associative — `(a ^ b)
    /// ^ c ≠ a ^ (b ^ c)` in general, so XOR cannot ride a
    /// flattened `parts: Vec<_>` like And/Or do. Mirrors Sassi's
    /// `BasicPredicate::Xor(Box, Box)` shape. The general-form SQL
    /// emit is `(NOT a AND b) OR (a AND NOT b)`; a boolean fast-path
    /// (`a <> b` when both operands are pre-evaluated booleans) is
    /// deferred to a future pass (see deliverables bullet 3).
    Xor(Box<Q<T>>, Box<Q<T>>),

    /// SQL `NOT (...)` over a non-Portable Q. Pure-Portable negation
    /// rides Sassi's `Not` (which collapses double-negation in place);
    /// this variant only exists for the SQL-only / mixed side.
    /// `!Q::Negated(inner)` collapses to `*inner` to avoid stacked
    /// `NOT NOT` nodes.
    Negated(Box<Q<T>>),
}

// Manual `Clone` for `Q<T>` — the derived `#[derive(Clone)]` would
// impose `T: Clone` because `BasicPredicate<T>` (carried inside
// `PortablePredicate<T>`) and other payloads use generic types
// indirectly. The manual implementation avoids this bound, so
// `QuerySet::clone` works for any `T: Model` regardless of whether
// the model derives `Clone`.
// Per-payload audit:
// - `Q::Portable(PortablePredicate<T>)` clones via PortablePredicate's
// manual `Clone` (no `T: Clone` bound).
// - `Q::Ilike(FieldRef<T, String>, String)` — `FieldRef<T, V>` is `Copy`
// (phantom-typed marker) and `String: Clone` — no `T: Clone`.
// - `Q::JsonbPath(JsonbPathLeaf)` — payload is `Clone` without `T`.
// - `Q::Regex(FieldRef, String, bool)` — same shape as `Ilike`.
// - `Q::Expression(Expr<bool>)` — `Expr<bool>: Clone`; the inner
// `ExprNode` is `Clone` via shallow `Box`/`Vec` clones. `SubqueryNode`
// payloads carry an `Arc<dyn SubqueryPredicateEmitter>`, so the
// model parameter never reaches a clone bound.
// - `Q::Array(ArrayPredicate<T>)` — `Clone` via `PhantomData<fn -> T>`
// plus owned leaf values; no `T: Clone` propagation.
// - `Q::Condition(Condition)` — `Condition: Clone` unconditionally.
// - `Q::Compound { op, parts }` — recurses through this manual impl.
// - `Q::Xor(Box<Q<T>>, Box<Q<T>>)` / `Q::Negated(Box<Q<T>>)` — recurse.
impl<T: Model> Clone for Q<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Portable(predicate) => Self::Portable(predicate.clone()),
            Self::Ilike(field, value) => Self::Ilike(*field, value.clone()),
            Self::JsonbPath(leaf) => Self::JsonbPath(leaf.clone()),
            Self::Regex(field, value, case_sensitive) => {
                Self::Regex(*field, value.clone(), *case_sensitive)
            }
            Self::Expression(expr) => Self::Expression(expr.clone()),
            Self::Array(array) => Self::Array(array.clone()),
            Self::Condition(condition) => Self::Condition(condition.clone()),
            Self::Compound { op, parts } => Self::Compound {
                op: *op,
                parts: parts.clone(),
            },
            Self::Xor(left, right) => {
                Self::Xor(Box::new((**left).clone()), Box::new((**right).clone()))
            }
            Self::Negated(inner) => Self::Negated(Box::new((**inner).clone())),
        }
    }
}

// Manual `Debug` for `Q<T>` — a derived `#[derive(Debug)]` would impose
// `T: Debug` because `PortablePredicate<T>` / `BasicPredicate<T>` /
// `ArrayPredicate<T>` all carry the type parameter through their
// payload graphs. This manual walker is the only place portable field
// predicates print their `(field_name, op)` shape without forcing every
// model to derive `Debug`.
impl<T: Model> std::fmt::Debug for Q<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Portable(predicate) => f.debug_tuple("Portable").field(predicate).finish(),
            Self::Ilike(field, value) => f
                .debug_tuple("Ilike")
                .field(&field.column())
                .field(value)
                .finish(),
            Self::JsonbPath(leaf) => f.debug_tuple("JsonbPath").field(leaf).finish(),
            Self::Regex(field, value, case_sensitive) => f
                .debug_tuple("Regex")
                .field(&field.column())
                .field(value)
                .field(case_sensitive)
                .finish(),
            Self::Expression(_) => f.debug_struct("Expression").finish_non_exhaustive(),
            Self::Array(array) => f.debug_tuple("Array").field(array).finish(),
            Self::Condition(condition) => f.debug_tuple("Condition").field(condition).finish(),
            Self::Compound { op, parts } => f
                .debug_struct("Compound")
                .field("op", op)
                .field("parts", parts)
                .finish(),
            Self::Xor(left, right) => f.debug_tuple("Xor").field(left).field(right).finish(),
            Self::Negated(inner) => f.debug_tuple("Negated").field(inner).finish(),
        }
    }
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
/// `PhantomData<T>` keeps `ArrayPredicate<T>` covariant in the model
/// type so it slots cleanly into `Q<T>` without affecting variance
/// elsewhere. The leaves themselves carry typed bind values
/// (`Vec<V>` flattened through `FilterValue::Array*`) and do not
/// reference `T`; the phantom marker exists purely for the algebra's
/// per-model parameterization.
/// `#[non_exhaustive]` so future array operators (`array_length`,
/// `cardinality`, custom GIN/GiST indexable ops) can be added
/// without breaking downstream pattern matches.
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

// Manual `Clone` / `Debug` for `ArrayPredicate<T>`.
// The derived versions impose `T: Clone` / `T: Debug` via the
// `PhantomData<T>` carriers, which would propagate virally through
// `Q<T>: Clone` / `Q<T>: Debug` and force every model to derive both.
// The leaf payloads themselves carry no `T`-bound state — they own
// only the column name and a typed bind vector — so manual impls are
// trivial and remove the unwanted bound.
impl<T: Model> Clone for ArrayPredicate<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Contains(leaf, _) => Self::Contains(leaf.clone(), std::marker::PhantomData),
            Self::ContainedBy(leaf, _) => Self::ContainedBy(leaf.clone(), std::marker::PhantomData),
            Self::Overlap(leaf, _) => Self::Overlap(leaf.clone(), std::marker::PhantomData),
        }
    }
}

impl<T: Model> std::fmt::Debug for ArrayPredicate<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Contains(leaf, _) => f.debug_tuple("Contains").field(leaf).finish(),
            Self::ContainedBy(leaf, _) => f.debug_tuple("ContainedBy").field(leaf).finish(),
            Self::Overlap(leaf, _) => f.debug_tuple("Overlap").field(leaf).finish(),
        }
    }
}

// ── `From` impls — array leaves lift into `ArrayPredicate<T>` then `Q<T>` ────
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

// ── `IntoQ<T>` — sealed trait for `filter_struct` / `exclude_struct` ─────────
// Anything convertible into a `Q<T>` for
// `QuerySet::filter_struct` / `QuerySet::exclude_struct` implements
// this trait. The sealing is the load-bearing piece: only djogi (and
// macro-emitted code in adopter crates) may extend the surface, so a
// downstream crate cannot reach for a custom impl that bypasses the
// `Q<T>` algebra invariants — the sealed trait is the type-system
// enforcement of the "no `Into<Condition>` ambiguity" invariant.
// The impl set:
// 1. `Q<T>` — identity. `filter_struct(my_q)` is the canonical caller.
// 2. `Condition` — legacy bridge for closure-side `f.col.eq(v)` callers
// that still produce `Condition`. Wraps as `Q::Condition(_)` so the
// SQL emitter sees the same tree the legacy path produced.
// 3. `PortablePredicate<T>` — Djogi-trusted Rust-evaluable predicate
// wrapper. Lifts to `Q::Portable(_)` so portable predicates flow
// through `QuerySet::filter` / `filter_struct` without requiring the
// caller to spell `Q::Portable(...)` at the callsite.
// 4. `Predicate<T>` — operator-matrix shell wrapping a `Q<T>`. Mixed
// `PortablePredicate` ↔ `Condition` compositions surface here.
// 5. `crate::expr::Expr<bool>` — typed expression boolean. Lifts to
// `Q::Expression(_)` so `f.location.explicit_pg_predicate.bounded_by(...)`
// composes through `QuerySet::filter` without a separate
// `filter_expr` call.
// 6. The `{Model}Filter` programmatic builder — emitted by the
// `#[derive(Model)]` macro alongside the existing `ModelFilter`
// impl. The bridge consumes the stored `FilterClause` vector and
// lazily reconstructs portable Q leaves for the conservative cases
// the macro can prove from model metadata. Unsupported fields,
// wrapped/optional shapes, value mismatches, and SQL-only operators
// fall back to `Q::Condition(_)`, so SQL behavior remains the
// compatibility floor without storing parallel predicate state.
// `IntoQ<T> for sassi::BasicPredicate<T>` and
// `From<sassi::BasicPredicate<T>> for Q<T>` are deliberately not exposed:
// raw Sassi predicates can pair forged column names with unrelated
// extractor closures (`Field::new("col_a", |x| &x.col_b)`), so trusting
// them at Djogi cache boundaries would let SQL emission and Punnu
// evaluation diverge. Adopters reach the wrapper through Djogi root
// field methods (`f.col.eq(v) -> PortablePredicate<T>`) instead.

mod sealed_into_q {
    /// Crate-private seal. Only djogi (and macro-emitted code routed
    /// through `crate::__private`) may impl `IntoQ<T>`.
    pub trait Sealed {}
}

/// Macro-only seal extension for `{Model}Filter` types.
/// `#[derive(Model)]` emits an `impl IntoQ<#model_ty> for #filter_name`
/// alongside the existing `ModelFilter` impl. To satisfy the
/// crate-private `sealed_into_q::Sealed` supertrait from a user crate,
/// the emitted code routes through `::djogi::__private::seal_model_filter_for_into_q!`
/// which expands to a single `impl Sealed for #filter_name` line.
/// Adopter code cannot call this macro directly — it lives in
/// `__private` and is only reachable from the proc-macro's emitted
/// output (per `feedback_macro_path_routing.md`).
#[doc(hidden)]
pub use sealed_into_q::Sealed as __SealedIntoQ;

/// Anything convertible into a [`Q<T>`] for
/// [`QuerySet::filter_struct`](crate::query::QuerySet::filter_struct)
/// / [`QuerySet::exclude_struct`](crate::query::QuerySet::exclude_struct).
/// Sealed — see `mod sealed_into_q`. The sealing closes the
/// "downstream `Into<Condition>` ambiguity" attack: a hostile downstream
/// impl cannot smuggle a non-`Q<T>` type through the filter API.
pub trait IntoQ<T: Model>: sealed_into_q::Sealed {
    /// Lower the implementor into the `Q<T>` algebra.
    fn into_q(self) -> Q<T>;
}

impl<T: Model> sealed_into_q::Sealed for Q<T> {}
impl<T: Model> IntoQ<T> for Q<T> {
    #[inline]
    fn into_q(self) -> Q<T> {
        self
    }
}

// Sealed `IntoQ<T> for Condition`.
// Lets the legacy closure-side filter API (`f.col.eq(v)` returning
// `Condition`) compose through the generalised `QuerySet::filter` /
// `filter_struct` signatures (`P: IntoQ<T>`). Wrapping as
// `Q::Condition(_)` preserves the SQL-parity contract: the lowering
// bridge round-trips `Q::Condition(_)` as the identity, so the SQL
// emitter sees the same `Condition` tree the closure-based path produced.
// The `Condition` type is locally owned by Djogi (`crate::query::condition`),
// so this impl satisfies Rust's orphan rules without reaching for the
// crate-private seal.
impl sealed_into_q::Sealed for Condition {}
impl<T: Model> IntoQ<T> for Condition {
    #[inline]
    fn into_q(self) -> Q<T> {
        Q::Condition(self)
    }
}

// Sealed `IntoQ<T> for Expr<bool>`.
// Keeps explicit-PG spatial expression predicates such as
// `f.location.explicit_pg_predicate.bounded_by(...)` and
// `f.location.explicit_pg_predicate.distance_to(&center).lt(5000.0)`
// on the ordinary `QuerySet::filter` path: the typed `Expr<bool>` lifts
// into `Q::Expression(_)`, the SQL emitter renders it through
// `expr::sql::emit_expr`, and `.try_portable` / `.cache(...)` /
// `.refresh_into(...)` reject `Q::Expression(_)` as cache-invalid.
// Sealed: `Expr<bool>` is local to Djogi (`crate::expr::Expr`), so the
// impl satisfies orphan rules without exposing the seal trait.
impl sealed_into_q::Sealed for crate::expr::Expr<bool> {}
impl<T: Model> IntoQ<T> for crate::expr::Expr<bool> {
    #[inline]
    fn into_q(self) -> Q<T> {
        Q::Expression(self)
    }
}

// ── Macro-emitted `IntoQ<T>` for `{Model}Filter` ────────────────────────────
// The `#[derive(Model)]` macro emits an `IntoQ<#model_ty>` impl for
// each `{Model}Filter` it generates. The impl keeps the filter's
// `FilterClause` vector as the single source of truth, reconstructs
// conservative portable leaves lazily, and uses `Q::Condition(_)` as
// the fallback for SQL-only clauses.
// The seal extension lives in `crate::__private::__seal_into_q_for_model_filter`
// so adopter crates cannot impl `IntoQ<T>` for arbitrary types — only
// the macro (which routes through that helper) and djogi itself reach
// the seal. See `djogi/src/lib.rs` for the helper definition.

// ── `Q<T>` constructors + operator overloads ─────────────────────────────────

impl<T: Model> Q<T> {
    /// Vacuous-truth identity — wraps a trusted-provenance Djogi
    /// [`PortablePredicate::always_true`] so unfiltered querysets stay
    /// SQL-emittable and Punnu-portable through the same wrapper as
    /// every other portable predicate.
    pub fn always_true() -> Self {
        Q::Portable(PortablePredicate::always_true())
    }

    /// Vacuous-falsehood identity — `Q::Portable(PortablePredicate::always_false)`.
    /// Used by `QuerySet::none`.
    pub fn always_false() -> Self {
        Q::Portable(PortablePredicate::always_false())
    }
}

// `From<sassi::BasicPredicate<T>> for Q<T>` is not exposed.
// Raw Sassi predicates carry attacker-controlled column names +
// arbitrary extractor closures, which would let SQL emission and Punnu
// evaluation diverge through the cache boundary. Djogi-trusted predicates
// flow through the typed root field surface (`DjogiField::eq(...) ->
// PortablePredicate<T>`) and from there into `Q<T>` via `IntoQ<T> for
// PortablePredicate<T>` (defined in `query::predicate`).

// `BitAnd` / `BitOr` / `BitXor` / `Not` impls — std already marks the
// trait methods `#[must_use]` (their result is the only meaningful
// product of the call), so re-adding the attribute on the impl
// methods is redundant and lints under rustc 1.95+. The `Q::*`
// constructors and `IntoQ::into_q` carry the `#[must_use]` that
// matters at adopter callsites.

impl<T: Model> BitAnd for Q<T> {
    type Output = Q<T>;
    /// SQL AND. Pure-Portable operands flatten through Sassi's
    /// `BasicPredicate::bitand` (via the trusted [`PortablePredicate`]
    /// wrapper) so chained `a & b & c` produces a single
    /// `Q::Portable(PortablePredicate::And(vec![a, b, c]))` — keeping the
    /// flattened-And invariant centralised in Sassi. Mixed operands lift
    /// to `Q::Compound { op: And, parts }` and flatten when either side
    /// is already a Compound-And.
    fn bitand(self, rhs: Self) -> Q<T> {
        compose_compound::<T, _>(self, rhs, CompoundOp::And, |a, b| a & b)
    }
}

impl<T: Model> BitOr for Q<T> {
    type Output = Q<T>;
    /// SQL OR. Dual of `BitAnd::bitand` — pure-Portable flattens through
    /// Sassi via the trusted wrapper; mixed operands lift to
    /// `Q::Compound { op: Or, parts }`.
    fn bitor(self, rhs: Self) -> Q<T> {
        compose_compound::<T, _>(self, rhs, CompoundOp::Or, |a, b| a | b)
    }
}

impl<T: Model> BitXor for Q<T> {
    type Output = Q<T>;
    /// SQL XOR. Pure-Portable operands ride Sassi's
    /// `BasicPredicate::bitxor` (via the trusted [`PortablePredicate`]
    /// wrapper, which produces a `BasicPredicate::Xor(Box, Box)`
    /// internally). Mixed operands lift to `Q::Xor(Box, Box)` directly.
    /// **Non-associative** — XOR chains do NOT flatten into a `Vec<_>`
    /// (unlike And/Or), so `(a ^ b) ^ c` is a left-leaning binary tree,
    /// not a 3-element flat node. Mirrors Sassi's choice
    /// (`BasicPredicate::Xor` is also binary).
    /// **Operator precedence reminder.** Rust binds `&` tighter than
    /// `^`, and `^` tighter than `|`. So
    /// `Q::Portable(...) ^ Q::Ilike(...) | Q::Expression(...)` parses as
    /// `(Q::Portable(...) ^ Q::Ilike(...)) | Q::Expression(...)`.
    /// A lihaaf compile-pass fixture locks this at the type level.
    fn bitxor(self, rhs: Self) -> Q<T> {
        match (self, rhs) {
            (Q::Portable(a), Q::Portable(b)) => Q::Portable(a ^ b),
            (lhs, rhs) => Q::Xor(Box::new(lhs), Box::new(rhs)),
        }
    }
}

impl<T: Model> Not for Q<T> {
    type Output = Q<T>;
    /// SQL `NOT (...)`. Pure-Portable operands ride Sassi's `Not` (via
    /// the trusted [`PortablePredicate`] wrapper), which collapses
    /// double-negation in place (`!!p == p`) and flips `True` ↔ `False`.
    /// Mixed operands wrap in `Q::Negated(...)`; `!Q::Negated(inner)`
    /// collapses to `*inner` to avoid stacked `NOT NOT` nodes that would
    /// emit redundant SQL parens. De Morgan's transformation is **not**
    /// applied across `Q::Compound` — the SQL emitter renders
    /// `Q::Negated(Q::Compound{...})` as `NOT (...)` directly, matching
    /// the existing `Condition::Not(Condition::And(...))` behavior.
    fn not(self) -> Q<T> {
        match self {
            Q::Portable(p) => Q::Portable(!p),
            Q::Negated(inner) => *inner,
            other => Q::Negated(Box::new(other)),
        }
    }
}

/// And/Or composition shared between `BitAnd` and `BitOr`. Pure-Portable
/// operands delegate to Sassi's flattening reducer (via the trusted
/// [`PortablePredicate`] wrapper, passed in as `portable_op`); mixed
/// operands lift to `Q::Compound { op, parts }` with the same flattening
/// contract Sassi uses internally:
/// `(Compound{op, parts: l}, Compound{op, parts: r})` extends `l` with
/// `r`, `(Compound{op, parts: l}, other)` pushes onto `l`, `(other,
/// Compound{op, parts: r})` prepends, and the bare-binary case wraps
/// `vec![lhs, rhs]`.
/// The `op` parameter is the Compound marker for the mixed path; the
/// `portable_op` parameter is the Sassi delegate for the pure-Portable
/// path. Splitting the two avoids a `match (lhs, rhs, op)` tower at each
/// callsite while keeping the shared shape DRY.
fn compose_compound<T: Model, F>(lhs: Q<T>, rhs: Q<T>, op: CompoundOp, portable_op: F) -> Q<T>
where
    F: FnOnce(PortablePredicate<T>, PortablePredicate<T>) -> PortablePredicate<T>,
{
    match (lhs, rhs) {
        (Q::Portable(a), Q::Portable(b)) => Q::Portable(portable_op(a, b)),
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
            let mut v = Vec::with_capacity(r.len() + 1);
            v.push(other);
            v.extend(r);
            Q::Compound { op, parts: v }
        }
        (lhs, rhs) => Q::Compound {
            op,
            parts: vec![lhs, rhs],
        },
    }
}

// ── `Q<T> → Condition` lowering bridge (legacy-only) ───────────────────────
// Production SQL emission uses `query::sql::emit_q` to walk `&Q<T>` directly,
// calling `query::portable::emit_portable_predicate` for `Q::Portable` leaves
// (which dispatches through `Model::__djogi_emit_field_predicate`). The bridge
// below lives on as an opt-in helper for legacy callers and tests that still
// inspect the lowered `Condition` shape.
// **Important caveat.** The `BasicPredicate::Field(_)` arm of the
// `q_to_condition` walker still panics (see `basic_predicate_to_condition`
// for the rationale — Sassi's `FieldPredicate::new` is `pub(crate)` so
// Djogi cannot reconstruct a `Condition::Leaf` here). Production SQL
// emission no longer reaches that arm because `emit_q` short-circuits
// `Q::Portable` through `query::portable::emit_portable_predicate`
// before any `Q -> Condition` lowering would happen. Tests that
// pattern-match on lowered `Condition` shape and only carry
// vacuous/structural variants (`True`, `False`, compound nodes, etc.)
// keep working through the bridge; tests that need to inspect a Field
// leaf must drive SQL emission through the direct walker instead.

/// Lower a [`Q<T>`] into the legacy [`Condition`] tree.
/// **Legacy-only.** The production SQL path uses `query::sql::emit_q` to
/// walk `&Q<T>` directly without ever building a `Condition` tree from a
/// portable predicate. This helper survives for legacy callers that still
/// inspect the lowered shape (queryset reducer, a handful of unit tests
/// that pre-date the direct walker).
/// # XOR general form
/// `Q::Xor(a, b)` lowers to `(NOT a AND b) OR (a AND NOT b)` — the
/// boolean fast-path (`a <> b`) is deferred to a future pass
/// (see deliverables §"Out-of-scope").
/// Same identity Sassi's `BasicPredicate::Xor` carries; the lowering
/// is identical whether the XOR rides `Q::Xor(_, _)` directly or
/// `Q::Portable(PortablePredicate::Xor(_, _))`.
#[allow(dead_code)]
pub(crate) fn q_to_condition<T: Model>(q: Q<T>) -> Condition {
    match q {
        Q::Portable(p) => basic_predicate_to_condition(p.into_inner()),
        Q::Ilike(field, pattern) => Condition::Leaf(Leaf::new(
            field.column(),
            LookupOp::IContains,
            FilterValue::String(pattern),
        )),
        Q::JsonbPath(leaf) => Condition::JsonbPath(leaf),
        Q::Regex(field, pattern, true) => Condition::Leaf(Leaf::new(
            field.column(),
            LookupOp::Regex,
            FilterValue::String(pattern),
        )),
        Q::Regex(field, pattern, false) => Condition::Leaf(Leaf::new(
            field.column(),
            LookupOp::IRegex,
            FilterValue::String(pattern),
        )),
        Q::Expression(expr) => Condition::Expr(expr),
        Q::Array(ArrayPredicate::Contains(leaf, _)) => Condition::ArrayContains(leaf),
        Q::Array(ArrayPredicate::ContainedBy(leaf, _)) => Condition::ArrayContainedBy(leaf),
        Q::Array(ArrayPredicate::Overlap(leaf, _)) => Condition::ArrayOverlap(leaf),
        // Identity — `Q::Condition(_)` is the escape hatch the legacy
        // path uses to feed a `Condition` into a `Q<T>` substrate.
        // Round-tripping it as the identity is what guarantees
        // character-for-character SQL parity post-substrate-flip.
        Q::Condition(c) => c,
        Q::Compound { op, parts } => {
            let lowered: Vec<Condition> = parts.into_iter().map(q_to_condition).collect();
            match op {
                CompoundOp::And => Condition::And(lowered),
                CompoundOp::Or => Condition::Or(lowered),
            }
        }
        Q::Xor(a, b) => xor_to_condition(*a, *b),
        Q::Negated(inner) => Condition::Not(Box::new(q_to_condition(*inner))),
        // `#[non_exhaustive]` catch-all for future Q<T> variants.
        // Panicking is correct — `Condition::True` would silently pass
        // every row. Update both `q_to_condition` and `q_to_condition_ref`
        // together when a new variant is added.
        #[allow(unreachable_patterns)]
        _ => panic!(
            "djogi internal: unhandled Q<T> variant in q_to_condition — \
             update the bridge when a new variant is added."
        ),
    }
}

/// Lower a sassi [`BasicPredicate`] into the legacy [`Condition`]
/// tree.
/// `BasicPredicate::Field(_)` is the type-erased pinch point: sassi's
/// `FieldPredicate` carries `Arc<dyn Any>` for its operand value, and
/// reconstructing the full [`FilterValue`] discriminant (including
/// `List` for `In` / `NotIn`, `Pair` for `Between`, etc.) from the
/// erased payload requires either:
/// 1. A model-side type registry mapping `(field_name, LookupOp)` to
///    the concrete value type, OR
/// 2. Each construction site lifting the value into [`FilterValue`]
///    before reaching the bridge.
///    Option 2 is the path djogi uses today — every typed `FieldRef`
///    lookup method (`eq`, `gt`, `ilike`, `between`, `in_list`, …)
///    returns [`Condition`] directly, so the
///    `BasicPredicate::Field(_)` arm of this match is **not reachable**
///    from any djogi FieldRef API as of Stage 2. A future
///    integration that lifts FieldRef methods to `BasicPredicate` (per
///    the §660 split's forward-looking direction in
///    `cluster-8gamma-granular.md`.8) would extend this arm with the
///    `(field_name, op, value_as<V>)` reconstruction.
///    Today the arm logs a debug-only warning and lowers to
///    `Condition::True` (vacuous-truth identity). The SQL-parity
///    SQL-parity guarantee is unaffected because no shipped code path
///    produces a `BasicPredicate::Field(_)` that flows through this
///    bridge.
fn basic_predicate_to_condition<T: Model>(bp: BasicPredicate<T>) -> Condition {
    match bp {
        BasicPredicate::True => Condition::True,
        // Empty `Or(vec![])` is the vacuous-falsehood identity — same
        // shape `Condition::or` uses, so SQL emission renders `FALSE`
        // (see `condition.rs:30` and `sql.rs:367`).
        BasicPredicate::False => Condition::Or(Vec::new()),
        BasicPredicate::And(parts) => Condition::And(
            parts
                .into_iter()
                .map(basic_predicate_to_condition)
                .collect(),
        ),
        BasicPredicate::Or(parts) => Condition::Or(
            parts
                .into_iter()
                .map(basic_predicate_to_condition)
                .collect(),
        ),
        BasicPredicate::Not(inner) => {
            Condition::Not(Box::new(basic_predicate_to_condition(*inner)))
        }
        BasicPredicate::Xor(a, b) => xor_to_condition_basic(*a, *b),
        BasicPredicate::Field(_fp) => {
            // `FieldPredicate::new` is `pub(crate)` in sassi so djogi
            // cannot read the field name / op / value to reconstruct a
            // `Condition::Leaf`. No djogi `FieldRef` method produces
            // `BasicPredicate::Field` today — they produce `Condition::Leaf`
            // via the closure filter path. Panicking is correct here:
            // `Condition::True` silently passes every row, which is far
            // more dangerous than a loud panic that surfaces the gap
            // immediately. Remove this panic when sassi exposes the
            // `FieldPredicate` fields so the arm can do a real
            // `Condition::Leaf` reconstruction.
            panic!(
                "djogi internal: cannot lower BasicPredicate::Field to \
                 Condition — FieldPredicate is pub(crate) in sassi. No djogi \
                 FieldRef API constructs this variant; reaching this panic \
                 means a future cluster must expose the sassi constructor. \
                 Use the closure-based filter API (QuerySet::filter) or \
                 Q<T> directly."
            )
        }
        // `#[non_exhaustive]` catch-all for future sassi BasicPredicate variants.
        // Panicking is correct — `Condition::True` would silently pass every
        // row. Update this arm when sassi adds a new variant.
        #[allow(unreachable_patterns)]
        _ => panic!(
            "djogi internal: unhandled BasicPredicate variant in \
             basic_predicate_to_condition — update the bridge when sassi \
             adds a new variant."
        ),
    }
}

/// XOR general-form lowering shared between `Q::Xor` and
/// `BasicPredicate::Xor`. Truth table identity:
/// `a XOR b ≡ (¬a ∧ b) ∨ (a ∧ ¬b)`.
fn xor_to_condition<T: Model>(a: Q<T>, b: Q<T>) -> Condition {
    let ca = q_to_condition(a);
    let cb = q_to_condition(b);
    Condition::Or(vec![
        Condition::And(vec![Condition::Not(Box::new(ca.clone())), cb.clone()]),
        Condition::And(vec![ca, Condition::Not(Box::new(cb))]),
    ])
}

/// XOR general-form for the sassi-side path.
/// Recursing into `basic_predicate_to_condition` keeps the lowering
/// scoped to the `BasicPredicate` subtree without round-tripping
/// through `q_to_condition`. The two helpers exist as a pair because
/// `Q::Xor(Box<Q<T>>, Box<Q<T>>)` and
/// `BasicPredicate::Xor(Box<BasicPredicate<T>>, Box<BasicPredicate<T>>)`
/// have different operand types and we lower each in its own arm
/// without an extra heap roundtrip.
fn xor_to_condition_basic<T: Model>(a: BasicPredicate<T>, b: BasicPredicate<T>) -> Condition {
    let ca = basic_predicate_to_condition(a);
    let cb = basic_predicate_to_condition(b);
    Condition::Or(vec![
        Condition::And(vec![Condition::Not(Box::new(ca.clone())), cb.clone()]),
        Condition::And(vec![ca, Condition::Not(Box::new(cb))]),
    ])
}

// ── Reference-borrowing lowering — `&Q<T> -> Condition` (legacy-only) ────────
// Production SQL emission uses direct Q walking: `emit_q` walks `&Q<T>` and
// emits `Q::Portable` leaves through the model hook
// (`Model::__djogi_emit_field_predicate`) without ever building a `Condition`
// shadow tree. The reference walker below is preserved as a legacy helper for
// unit tests that still inspect the lowered `Condition` shape.
// The `BasicPredicate::Field(_)` arm panics through
// `basic_predicate_ref_to_condition` (Sassi's `FieldPredicate::new` is
// `pub(crate)`, so Djogi cannot reconstruct a `Condition::Leaf` here).
// Production SQL emission never reaches the panic because direct-Q
// emission short-circuits `Q::Portable` first.

#[allow(dead_code)]
pub(crate) fn q_to_condition_ref<T: Model>(q: &Q<T>) -> Condition {
    match q {
        Q::Portable(p) => basic_predicate_ref_to_condition(p.inner_ref()),
        Q::Ilike(field, pattern) => Condition::Leaf(Leaf::new(
            field.column(),
            LookupOp::IContains,
            FilterValue::String(pattern.clone()),
        )),
        Q::JsonbPath(leaf) => Condition::JsonbPath(leaf.clone()),
        Q::Regex(field, pattern, true) => Condition::Leaf(Leaf::new(
            field.column(),
            LookupOp::Regex,
            FilterValue::String(pattern.clone()),
        )),
        Q::Regex(field, pattern, false) => Condition::Leaf(Leaf::new(
            field.column(),
            LookupOp::IRegex,
            FilterValue::String(pattern.clone()),
        )),
        Q::Expression(expr) => Condition::Expr(expr.clone()),
        Q::Array(ArrayPredicate::Contains(leaf, _)) => Condition::ArrayContains(leaf.clone()),
        Q::Array(ArrayPredicate::ContainedBy(leaf, _)) => Condition::ArrayContainedBy(leaf.clone()),
        Q::Array(ArrayPredicate::Overlap(leaf, _)) => Condition::ArrayOverlap(leaf.clone()),
        Q::Condition(c) => c.clone(),
        Q::Compound { op, parts } => {
            let lowered: Vec<Condition> = parts.iter().map(q_to_condition_ref).collect();
            match op {
                CompoundOp::And => Condition::And(lowered),
                CompoundOp::Or => Condition::Or(lowered),
            }
        }
        Q::Xor(a, b) => xor_to_condition_ref(a, b),
        Q::Negated(inner) => Condition::Not(Box::new(q_to_condition_ref(inner))),
        // `#[non_exhaustive]` catch-all for future Q<T> variants.
        // Panicking is correct — `Condition::True` would silently pass
        // every row. Update both `q_to_condition` and `q_to_condition_ref`
        // together when a new variant is added.
        #[allow(unreachable_patterns)]
        _ => panic!(
            "djogi internal: unhandled Q<T> variant in q_to_condition_ref — \
             update the bridge when a new variant is added."
        ),
    }
}

/// Reference-borrowing analogue of `basic_predicate_to_condition`.
/// Walks `&BasicPredicate<T>` and produces a fresh [`Condition`]
/// without invoking `Clone` on the predicate or its `T` parameter.
fn basic_predicate_ref_to_condition<T: Model>(bp: &BasicPredicate<T>) -> Condition {
    match bp {
        BasicPredicate::True => Condition::True,
        BasicPredicate::False => Condition::Or(Vec::new()),
        BasicPredicate::And(parts) => {
            Condition::And(parts.iter().map(basic_predicate_ref_to_condition).collect())
        }
        BasicPredicate::Or(parts) => {
            Condition::Or(parts.iter().map(basic_predicate_ref_to_condition).collect())
        }
        BasicPredicate::Not(inner) => {
            Condition::Not(Box::new(basic_predicate_ref_to_condition(inner)))
        }
        BasicPredicate::Xor(a, b) => xor_basic_ref_to_condition(a, b),
        BasicPredicate::Field(_fp) => {
            // Same rationale as `basic_predicate_to_condition`: `FieldPredicate`
            // is `pub(crate)` in sassi; panicking is correct over silent
            // `Condition::True`. See the owned bridge for the full explanation.
            panic!(
                "djogi internal: cannot lower BasicPredicate::Field to \
                 Condition (by-ref path) — FieldPredicate is pub(crate) in \
                 sassi. Use the closure-based filter API or Q<T> directly."
            )
        }
        #[allow(unreachable_patterns)]
        _ => panic!(
            "djogi internal: unhandled BasicPredicate variant in \
             basic_predicate_ref_to_condition — update the bridge when \
             sassi adds a new variant."
        ),
    }
}

/// XOR general-form for the reference-borrowing path. Same identity
/// as `xor_to_condition`: `(NOT a AND b) OR (a AND NOT b)`.
fn xor_to_condition_ref<T: Model>(a: &Q<T>, b: &Q<T>) -> Condition {
    let ca = q_to_condition_ref(a);
    let cb = q_to_condition_ref(b);
    Condition::Or(vec![
        Condition::And(vec![Condition::Not(Box::new(ca.clone())), cb.clone()]),
        Condition::And(vec![ca, Condition::Not(Box::new(cb))]),
    ])
}

/// XOR general-form for the sassi-side reference-borrowing path.
fn xor_basic_ref_to_condition<T: Model>(a: &BasicPredicate<T>, b: &BasicPredicate<T>) -> Condition {
    let ca = basic_predicate_ref_to_condition(a);
    let cb = basic_predicate_ref_to_condition(b);
    Condition::Or(vec![
        Condition::And(vec![Condition::Not(Box::new(ca.clone())), cb.clone()]),
        Condition::And(vec![ca, Condition::Not(Box::new(cb))]),
    ])
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
    // by `query::field::tests` — every async hook is `unreachable!`
    // because the algebra-level tests never invoke them. The empty
    // `Fields = ` tuple is enough; `Q::Portable` / `Q::Ilike` etc.
    // only need `M: Model`, not a populated field accessor surface.
    // The manual `Q<T>: Clone` and `PortablePredicate<T>: Clone` impls do
    // NOT propagate `T: Clone`, so `TestModel` derives `Clone` and `Debug`
    // for backwards compatibility with test bodies that print `Q<T>` payloads.
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

    // Helpers for the algebra tests below. All `Q<T>` construction in tests
    // goes through `Q::Portable(_)` with a `PortablePredicate<T>` payload.
    // `PortablePredicate::always_true` / `always_false` are crate-private
    // constructors that mint trusted-provenance vacuous wrappers without
    // going through the `DjogiField` surface.
    fn portable_true<T: Model>() -> Q<T> {
        Q::Portable(PortablePredicate::<T>::always_true())
    }

    fn portable_false<T: Model>() -> Q<T> {
        Q::Portable(PortablePredicate::<T>::always_false())
    }

    /// `Q::Portable` constructs from a `PortablePredicate<T>` — the
    /// load-bearing path for the §660 split.
    #[test]
    fn q_skeleton_constructs_portable_variant() {
        let q: Q<TestModel> = portable_true();
        assert!(matches!(q, Q::Portable(_)));
    }

    /// `Q::Portable(PortablePredicate::always_false)` is the vacuous-falsehood
    /// identity. Locks the contract that `Q<T>` does not duplicate
    /// True/False as separate top-level variants.
    #[test]
    fn q_skeleton_carries_portable_false() {
        let q: Q<TestModel> = portable_false();
        assert!(matches!(q, Q::Portable(_)));
    }

    /// `Clone` and `Debug` sanity check on the manual impls. The derived
    /// versions would impose `T: Clone` / `T: Debug` virally; the manual
    /// walkers cover every payload without that bound.
    #[test]
    fn q_skeleton_is_clone_and_debug() {
        let q: Q<TestModel> = portable_true();
        let _ = format!("{:?}", q.clone());
    }

    /// Convenience constructor — `Q::always_true` produces a trusted
    /// portable-true wrapper.
    #[test]
    fn q_always_true_is_portable_true() {
        let q: Q<TestModel> = Q::always_true();
        assert!(matches!(q, Q::Portable(_)));
    }

    /// Dual: `always_false` is `Q::Portable(PortablePredicate::always_false)`.
    #[test]
    fn q_always_false_is_portable_false() {
        let q: Q<TestModel> = Q::always_false();
        assert!(matches!(q, Q::Portable(_)));
    }

    /// Pure-Portable AND short-circuits through Sassi (via the trusted
    /// wrapper) — the resulting `Q::Portable` carries a flattened
    /// `BasicPredicate::And(_)` underneath.
    #[test]
    fn q_portable_and_portable_flattens_through_sassi() {
        let a: Q<TestModel> = portable_true();
        let b: Q<TestModel> = portable_false();
        match a & b {
            Q::Portable(p) => match p.into_inner() {
                BasicPredicate::And(parts) => {
                    assert_eq!(parts.len(), 2);
                    assert!(matches!(parts[0], BasicPredicate::True));
                    assert!(matches!(parts[1], BasicPredicate::False));
                }
                other => panic!("expected BasicPredicate::And(_), got {other:?}"),
            },
            other => panic!("expected Q::Portable(_), got {other:?}"),
        }
    }

    /// Pure-Portable OR short-circuits through Sassi as well.
    #[test]
    fn q_portable_or_portable_flattens_through_sassi() {
        let a: Q<TestModel> = portable_true();
        let b: Q<TestModel> = portable_false();
        match a | b {
            Q::Portable(p) => match p.into_inner() {
                BasicPredicate::Or(_) => {}
                other => panic!("expected BasicPredicate::Or(_), got {other:?}"),
            },
            other => panic!("expected Q::Portable(_), got {other:?}"),
        }
    }

    /// Pure-Portable XOR rides Sassi's `BasicPredicate::Xor(Box, Box)`.
    /// No flattening — XOR is non-associative.
    #[test]
    fn q_portable_xor_portable_uses_sassi_xor() {
        let a: Q<TestModel> = portable_true();
        let b: Q<TestModel> = portable_false();
        match a ^ b {
            Q::Portable(p) => match p.into_inner() {
                BasicPredicate::Xor(_, _) => {}
                other => panic!("expected BasicPredicate::Xor(_, _), got {other:?}"),
            },
            other => panic!("expected Q::Portable(_), got {other:?}"),
        }
    }

    /// Pure-Portable double-negation collapses through Sassi's `Not`
    /// reducer (`!!p == p`). The outer `Q::Portable` wraps the
    /// already-collapsed Sassi result; no `Q::Negated` should appear.
    #[test]
    fn q_portable_double_negation_collapses_via_sassi() {
        let p: Q<TestModel> = portable_true();
        let result = !!p;
        match result {
            Q::Portable(p) => match p.into_inner() {
                BasicPredicate::True => {}
                other => panic!("expected BasicPredicate::True, got {other:?}"),
            },
            other => panic!("expected Q::Portable(_) after !!, got {other:?}"),
        }
    }

    /// Mixed-operand AND lifts to `Q::Compound`.
    #[test]
    fn q_mixed_and_creates_compound_node() {
        let a: Q<TestModel> = portable_true();
        let b: Q<TestModel> = Q::Negated(Box::new(portable_false()));
        match a & b {
            Q::Compound {
                op: CompoundOp::And,
                parts,
            } => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(parts[0], Q::Portable(_)));
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
        // (which prevents the pure-Portable short-circuit).
        let neg = || Q::<TestModel>::Negated(Box::new(portable_true()));
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
        let neg = || Q::<TestModel>::Negated(Box::new(portable_true()));
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
        let basic: Q<TestModel> = portable_true();
        let neg: Q<TestModel> = Q::Negated(Box::new(portable_false()));
        match basic ^ neg {
            Q::Xor(lhs, rhs) => {
                assert!(matches!(*lhs, Q::Portable(_)));
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
        // Wrap something non-Portable so the negation lands in `Q::Negated`
        // rather than collapsing through Sassi.
        let inner: Q<TestModel> = Q::Compound {
            op: CompoundOp::And,
            parts: vec![
                Q::Negated(Box::new(portable_true())),
                Q::Negated(Box::new(portable_false())),
            ],
        };
        let result = !!inner.clone();
        // After two `Not::not` calls: first wraps in `Q::Negated`, second
        // unwraps. Result is the original non-Portable inner.
        assert!(matches!(result, Q::Compound { .. }));
    }

    /// Operator precedence runtime check. Locks Rust's table:
    /// `&` > `^` > `|`. So `Q::Portable(_) ^ Q::Negated(...) | Q::Negated(...)`
    /// parses as `(Q::Portable(_) ^ Q::Negated(...)) | Q::Negated(...)`.
    /// A lihaaf compile-pass doubles this with a
    /// type-level lock; this runtime test validates the resulting
    /// `Q` shape.
    #[test]
    fn q_operator_precedence_xor_binds_tighter_than_or() {
        let a: Q<TestModel> = portable_true();
        let b: Q<TestModel> = Q::Negated(Box::new(portable_false()));
        let c: Q<TestModel> = Q::Negated(Box::new(portable_true()));

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
    /// to extend this match (and the SQL emitter).
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
        let a: Q<TestModel> = portable_true();
        let b: Q<TestModel> = Q::Negated(Box::new(portable_false()));
        let c: Q<TestModel> = Q::Negated(Box::new(portable_true()));

        let composed = a & b ^ c;

        // Outer Xor.
        match composed {
            Q::Xor(lhs, rhs) => {
                // Left half: the AND result. With one Portable and one
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

    // ── Operator-precedence tree-shape assertions ─────────────────────

    /// `a & b ^ c | d` must parse as `((a & b) ^ c) | d` under Rust's
    /// operator precedence rules: `&` binds tighter than `^`, which binds
    /// tighter than `|`. Compile-pass lihaaf fixtures verify the expression
    /// compiles; this test verifies the resulting tree shape.
    /// When all operands are `Q::Portable(...)`, the `BitAnd`/`BitOr`/`BitXor`
    /// impls delegate to Sassi's `BasicPredicate` operators (via the trusted
    /// wrapper), producing a
    /// `Q::Portable(PortablePredicate::Or([Xor(And([...]), ...), ...]))`
    /// tree. Checking the inner `BasicPredicate` structure verifies precedence.
    /// Locking this prevents a future operator-impl rewrite from silently
    /// changing associativity without this test catching it.
    #[test]
    fn operator_precedence_a_and_b_xor_c_or_d() {
        let a: Q<TestModel> = Q::always_true();
        let b: Q<TestModel> = Q::always_false();
        let c: Q<TestModel> = Q::always_true();
        let d: Q<TestModel> = Q::always_false();

        // `a & b ^ c | d` = `((a & b) ^ c) | d`
        let result = a & b ^ c | d;

        // All operands are Q::Portable → result is Q::Portable wrapping the
        // BasicPredicate algebra tree. Outer node must be Or.
        match result {
            Q::Portable(p) => match p.into_inner() {
                BasicPredicate::Or(or_parts) => {
                    assert_eq!(or_parts.len(), 2, "outer Or must have 2 branches");
                    // Left branch: `(a & b) ^ c` = Xor(And([True, False]), True)
                    match &or_parts[0] {
                        BasicPredicate::Xor(lhs, rhs_c) => {
                            // XOR RHS is `c` = BasicPredicate::True
                            assert!(
                                matches!(rhs_c.as_ref(), BasicPredicate::True),
                                "XOR rhs must be c = True, got {rhs_c:?}"
                            );
                            // XOR LHS is `a & b` = And([True, False])
                            match lhs.as_ref() {
                                BasicPredicate::And(and_parts) => {
                                    assert_eq!(and_parts.len(), 2, "And must have 2 parts (a, b)");
                                    assert!(
                                        matches!(and_parts[0], BasicPredicate::True),
                                        "And[0] must be a = True"
                                    );
                                    assert!(
                                        matches!(and_parts[1], BasicPredicate::False),
                                        "And[1] must be b = False"
                                    );
                                }
                                other => {
                                    panic!("expected And([True, False]) as XOR lhs, got {other:?}")
                                }
                            }
                        }
                        other => panic!("expected Xor as left Or branch, got {other:?}"),
                    }
                    // Right branch: `d` = BasicPredicate::False
                    assert!(
                        matches!(or_parts[1], BasicPredicate::False),
                        "right Or branch must be d = False"
                    );
                }
                other => panic!("expected BasicPredicate::Or([...]), got {other:?}"),
            },
            other => panic!("expected Q::Portable(_), got {other:?}"),
        }
    }

    /// Eight-term composition `a & b & c & d | e | f ^ g ^ h` verifies that
    /// the `BitAnd` / `BitOr` / `BitXor` impls flatten consecutive same-op
    /// calls (the `And`-chain has 4 parts, not a nested binary tree).
    /// When all operands are `Q::Portable`, operators delegate to Sassi's
    /// `BasicPredicate` algebra (via the trusted wrapper) — the flattening
    /// happens at the `BasicPredicate` level. Outermost node is Or.
    #[test]
    fn eight_term_composition_flattens_same_op() {
        let a: Q<TestModel> = Q::always_true();
        let b: Q<TestModel> = Q::always_false();
        let c: Q<TestModel> = Q::always_true();
        let d: Q<TestModel> = Q::always_false();
        let e: Q<TestModel> = Q::always_true();
        let f: Q<TestModel> = Q::always_false();
        let g: Q<TestModel> = Q::always_true();
        let h: Q<TestModel> = Q::always_false();

        // Precedence: `&` first, then `^`, then `|`
        // = (a & b & c & d) | e | (f ^ g ^ h)
        let result = a & b & c & d | e | f ^ g ^ h;

        match result {
            Q::Portable(p) => match p.into_inner() {
                BasicPredicate::Or(or_parts) => {
                    assert!(
                        or_parts.len() >= 2,
                        "outer Or must have ≥ 2 parts; got {}",
                        or_parts.len()
                    );
                    // The And-chain `a & b & c & d` should appear as one
                    // Or-part with 4 inner parts (flattened, not binary-
                    // nested).
                    let has_4_way_and = or_parts
                        .iter()
                        .any(|p| matches!(p, BasicPredicate::And(ap) if ap.len() == 4));
                    assert!(
                        has_4_way_and,
                        "expected a 4-part And in Or parts (a&b&c&d flattened); parts: {or_parts:?}"
                    );
                }
                other => panic!("expected BasicPredicate::Or([...]), got {other:?}"),
            },
            other => panic!("expected Q::Portable(_), got {other:?}"),
        }
    }

    // ── Q→Condition lowering bridge tests ────────────────────────────────────────────
    // These tests lock the `Q<T> → Condition` lowering at every variant.
    // Together with the integration suite, they guarantee character-for-
    // character SQL parity at the substrate flip: every variant the
    // legacy `Condition` path could carry round-trips through
    // `q_to_condition` to the same shape the SQL emitter consumed before.

    /// `Q::Portable(PortablePredicate::True)` lowers to `Condition::True`
    /// through the legacy bridge.
    #[test]
    fn q_portable_true_lowers_to_condition_true() {
        let q: Q<TestModel> = portable_true();
        assert!(matches!(q_to_condition(q), Condition::True));
    }

    /// `Q::Portable(PortablePredicate::False)` lowers to the empty-Or
    /// vacuous-falsehood identity (matches `condition.rs:30` and the
    /// SQL emitter's `FALSE` rendering at `sql.rs:367`) through the
    /// legacy bridge.
    #[test]
    fn q_portable_false_lowers_to_empty_or() {
        let q: Q<TestModel> = portable_false();
        match q_to_condition(q) {
            Condition::Or(v) => assert!(v.is_empty(), "expected empty Or, got {v:?}"),
            other => panic!("expected Condition::Or(empty), got {other:?}"),
        }
    }

    /// `Q::Compound { op: And, parts }` lowers to `Condition::And` in
    /// the same order — locks the order-preservation contract the SQL
    /// emitter relies on for predictable EXPLAIN output.
    #[test]
    fn q_compound_and_lowers_preserves_order() {
        // Use Q::Condition wrappers so we can compare leaves by column name.
        let a =
            Q::<TestModel>::Condition(Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true))));
        let b =
            Q::<TestModel>::Condition(Condition::Leaf(Leaf::eq_raw("b", FilterValue::Bool(false))));
        let c =
            Q::<TestModel>::Condition(Condition::Leaf(Leaf::eq_raw("c", FilterValue::Bool(true))));
        let q = Q::Compound {
            op: CompoundOp::And,
            parts: vec![a, b, c],
        };
        match q_to_condition(q) {
            Condition::And(parts) => {
                assert_eq!(parts.len(), 3);
                let names: Vec<&'static str> = parts
                    .iter()
                    .map(|p| match p {
                        Condition::Leaf(l) => l.column(),
                        _ => panic!("expected leaf, got {p:?}"),
                    })
                    .collect();
                assert_eq!(names, vec!["a", "b", "c"]);
            }
            other => panic!("expected Condition::And, got {other:?}"),
        }
    }

    /// `Q::Compound { op: Or, parts }` lowers to `Condition::Or`.
    #[test]
    fn q_compound_or_lowers_to_or() {
        let q = Q::<TestModel>::Compound {
            op: CompoundOp::Or,
            parts: vec![portable_true(), portable_false()],
        };
        match q_to_condition(q) {
            Condition::Or(parts) => assert_eq!(parts.len(), 2),
            other => panic!("expected Condition::Or, got {other:?}"),
        }
    }

    /// `Q::Negated(inner)` lowers to `Condition::Not(_)`.
    #[test]
    fn q_negated_lowers_to_condition_not() {
        let q = Q::<TestModel>::Negated(Box::new(portable_true()));
        match q_to_condition(q) {
            Condition::Not(inner) => assert!(matches!(*inner, Condition::True)),
            other => panic!("expected Condition::Not, got {other:?}"),
        }
    }

    /// `Q::Xor(a, b)` lowers to the general form
    /// `(NOT a AND b) OR (a AND NOT b)` — Sassi's identity, no
    /// boolean fast-path (deferred to a future pass).
    #[test]
    fn q_xor_lowers_to_general_form() {
        let a = portable_true::<TestModel>();
        let b = portable_false::<TestModel>();
        let q = Q::Xor(Box::new(a), Box::new(b));
        match q_to_condition(q) {
            Condition::Or(or_parts) => {
                assert_eq!(or_parts.len(), 2, "outer Or must have 2 branches");
                // First branch: (NOT a AND b)
                match &or_parts[0] {
                    Condition::And(and_parts) => {
                        assert_eq!(and_parts.len(), 2);
                        assert!(matches!(and_parts[0], Condition::Not(_)));
                    }
                    other => panic!("expected And as first Or branch, got {other:?}"),
                }
                // Second branch: (a AND NOT b)
                match &or_parts[1] {
                    Condition::And(and_parts) => {
                        assert_eq!(and_parts.len(), 2);
                        assert!(matches!(and_parts[1], Condition::Not(_)));
                    }
                    other => panic!("expected And as second Or branch, got {other:?}"),
                }
            }
            other => panic!("expected Condition::Or for XOR general form, got {other:?}"),
        }
    }

    /// `Q::Regex(field, pattern, true)` lowers to `LookupOp::Regex`
    /// the load-bearing test for the §660 split. Confirms regex
    /// stays SQL-only and never reaches sassi's `BasicPredicate`.
    #[test]
    fn q_regex_case_sensitive_lowers_to_lookup_op_regex() {
        // Construct a FieldRef via the macro support helper. The
        // typed FieldRef API requires a field name + the model
        // type; using a string column name + LookupOp directly via
        // the lowering bridge is the only way to test this without
        // standing up a full model.
        // Skipping FieldRef construction here — instead, directly
        // exercise the `Q::Ilike` / `Q::Regex` arms via the lowering
        // function with a leaf the bridge produces. The covering
        // `q_regex_*` tests below assert via the resulting
        // Condition::Leaf shape.
        use crate::query::field::__macro_support::__make_field_ref;
        let field: FieldRef<TestModel, String> = __make_field_ref(None, "slug");
        let q: Q<TestModel> = Q::Regex(field, "^foo".to_string(), true);
        match q_to_condition(q) {
            Condition::Leaf(leaf) => {
                assert_eq!(leaf.op(), LookupOp::Regex);
                assert_eq!(leaf.column(), "slug");
                assert!(matches!(leaf.value(), FilterValue::String(s) if s == "^foo"));
            }
            other => panic!("expected Condition::Leaf with LookupOp::Regex, got {other:?}"),
        }
    }

    /// `Q::Regex(field, pattern, false)` lowers to `LookupOp::IRegex`
    /// the case-insensitive POSIX regex variant.
    #[test]
    fn q_regex_case_insensitive_lowers_to_lookup_op_iregex() {
        use crate::query::field::__macro_support::__make_field_ref;
        let field: FieldRef<TestModel, String> = __make_field_ref(None, "slug");
        let q: Q<TestModel> = Q::Regex(field, "^foo".to_string(), false);
        match q_to_condition(q) {
            Condition::Leaf(leaf) => {
                assert_eq!(leaf.op(), LookupOp::IRegex);
            }
            other => panic!("expected Condition::Leaf with LookupOp::IRegex, got {other:?}"),
        }
    }

    /// `Q::Ilike(field, pattern)` lowers to `LookupOp::IContains`
    /// (the case-insensitive ILIKE family the existing FieldRef
    /// `.contains` API also routes through).
    #[test]
    fn q_ilike_lowers_to_lookup_op_icontains() {
        use crate::query::field::__macro_support::__make_field_ref;
        let field: FieldRef<TestModel, String> = __make_field_ref(None, "title");
        let q: Q<TestModel> = Q::Ilike(field, "rust%".to_string());
        match q_to_condition(q) {
            Condition::Leaf(leaf) => {
                assert_eq!(leaf.op(), LookupOp::IContains);
                assert_eq!(leaf.column(), "title");
            }
            other => panic!("expected Condition::Leaf with LookupOp::IContains, got {other:?}"),
        }
    }

    /// `Q::Condition(c)` round-trips as the identity — load-bearing
    /// for character-for-character SQL parity. Every legacy
    /// `Condition` lifted through this variant produces the **same**
    /// SQL the pre-flip code path produced.
    #[test]
    fn q_condition_round_trips_as_identity() {
        let original = Condition::Leaf(Leaf::eq_raw("status", FilterValue::Bool(true)));
        let q: Q<TestModel> = Q::Condition(original.clone());
        match q_to_condition(q) {
            Condition::Leaf(leaf) => {
                assert_eq!(leaf.column(), "status");
                assert_eq!(leaf.op(), LookupOp::Eq);
            }
            other => panic!("expected identity round-trip, got {other:?}"),
        }
    }

    /// `Q::Array(ArrayPredicate::Contains(...))` lowers to the
    /// matching `Condition::ArrayContains` variant.
    #[test]
    fn q_array_contains_lowers_to_condition_array_contains() {
        use crate::array::ArrayContainsLeaf;
        let leaf = ArrayContainsLeaf {
            column: "tags",
            values: FilterValue::ArrayString(vec!["a".to_string()]),
        };
        let q: Q<TestModel> = leaf.into();
        match q_to_condition(q) {
            Condition::ArrayContains(l) => assert_eq!(l.column, "tags"),
            other => panic!("expected Condition::ArrayContains, got {other:?}"),
        }
    }

    /// `Q::Array(ArrayPredicate::ContainedBy(...))` lowers identically.
    #[test]
    fn q_array_contained_by_lowers_to_condition_array_contained_by() {
        use crate::array::ArrayContainedByLeaf;
        let leaf = ArrayContainedByLeaf {
            column: "tags",
            values: FilterValue::ArrayI32(vec![1, 2]),
        };
        let q: Q<TestModel> = leaf.into();
        assert!(matches!(q_to_condition(q), Condition::ArrayContainedBy(_)));
    }

    /// `Q::Array(ArrayPredicate::Overlap(...))` lowers identically.
    #[test]
    fn q_array_overlap_lowers_to_condition_array_overlap() {
        use crate::array::ArrayOverlapLeaf;
        let leaf = ArrayOverlapLeaf {
            column: "tags",
            values: FilterValue::ArrayBool(vec![true]),
        };
        let q: Q<TestModel> = leaf.into();
        assert!(matches!(q_to_condition(q), Condition::ArrayOverlap(_)));
    }

    /// Sassi `BasicPredicate::And/Or/Not` round-trip through the
    /// `Q::Portable` wrapper into `Condition::And/Or/Not` with the same
    /// flattening rules — no extra wrapping, no implicit re-association.
    /// Uses `PortablePredicate`'s pure-portable boolean composition (`&`
    /// / `|` / `!`) so trusted-provenance vacuous wrappers compose
    /// without reaching for raw `BasicPredicate<T>`.
    #[test]
    fn q_portable_and_or_not_round_trip() {
        let composed: PortablePredicate<TestModel> =
            PortablePredicate::always_true() & PortablePredicate::always_false();
        let q: Q<TestModel> = Q::Portable(composed);
        match q_to_condition(q) {
            Condition::And(parts) => assert_eq!(parts.len(), 2),
            other => panic!("expected Condition::And, got {other:?}"),
        }

        let negated: PortablePredicate<TestModel> = !PortablePredicate::always_true();
        let q: Q<TestModel> = Q::Portable(negated);
        // `!True` collapses through Sassi to `BasicPredicate::False`,
        // which the legacy bridge lowers to `Condition::Or(empty)`.
        match q_to_condition(q) {
            Condition::Or(parts) => assert!(parts.is_empty()),
            other => panic!("expected Condition::Or(empty), got {other:?}"),
        }
    }
}
