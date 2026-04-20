//! Internal expression AST node — the untyped dynamic payload behind the
//! typed [`Expr<T>`](super::Expr) wrapper.
//!
//! # What
//!
//! [`ExprNode`] is an untyped enum tree: leaves are `Field { column }` /
//! `Literal(FilterValue)`, internal nodes are arithmetic, comparison, and
//! (in later phases) subquery / aggregate / CASE forms. The typed
//! [`Expr<T>`](super::Expr) wrapper projects `T`-safety over this dynamic
//! core so user-facing APIs stay typed while the SQL emitter only has one
//! variant tree to walk.
//!
//! # Why separate the typed wrapper from the node?
//!
//! - **One emitter walk.** [`super::sql::emit_expr`] matches this enum
//!   exhaustively. If `ExprNode` were polymorphic in `T`, every emitter
//!   call site would monomorphise per-type; by erasing `T` at the enum
//!   boundary we get one codegen path and one set of tests.
//! - **Arithmetic composition.** `Expr<i32> + Expr<i32>` yields `Expr<i32>`
//!   — the typed wrapper enforces the operator is only available for
//!   numeric `T`, but the node it wraps stores a plain `Add(Box<_>, Box<_>)`
//!   regardless of `T`. Same pattern for comparisons (`Expr<T>.eq(Expr<T>)
//!   -> Expr<bool>` — the wrapper changes `T` from `T` to `bool`, the node
//!   is a `Cmp { op: Eq, .. }`).
//! - **Phase expansion.** Tasks 4 / 5 add `Case`, `Exists`, `Subquery`,
//!   `Aggregate`, and `OuterRef` variants. Keeping the enum untyped means
//!   those additions don't ripple into every type-parameterised site; only
//!   the emitter and a few typed constructors grow.
//!
//! # Where
//!
//! - [`super::Expr`] — typed wrapper, the public surface.
//! - [`super::sql::emit_expr`] — the matching emitter (one arm per variant).
//! - [`crate::query::condition::Condition::Expr`] — the bridge that promotes
//!   an `Expr<bool>` into the filter tree.

use crate::query::condition::FilterValue;

// TODO(Phase 4 Task 5): extend this enum with `Case { arms, otherwise }`,
// `Exists(..)`, `Subquery(..)`, `OuterRef { column }`. Phase 4 Task 4 landed
// the `Aggregate` variant below. The emitter in `expr::sql` needs matching
// arms in lockstep. The `#[non_exhaustive]` marker is crate-private today
// (same-crate matches are already exhaustive); it becomes relevant if/when
// this enum is re-exported.

/// Untyped expression tree. The typed [`super::Expr<T>`] wrapper carries
/// the phantom `T` parameter and projects type safety over the dynamic
/// variants stored here. The SQL emitter walks this enum directly.
///
/// Marked `#[non_exhaustive]` to leave room for Phase 4 Tasks 4 / 5
/// additions (`Case`, `Exists`, `Subquery`, `Aggregate`, `OuterRef`) without
/// forcing a downstream semver bump. `Condition::Expr` carries `Expr<bool>`,
/// not `ExprNode`, so the typed seal stays intact at the public boundary.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) enum ExprNode {
    /// Bare column reference. The `column` string is a `&'static str`
    /// that was validated at [`crate::query::field::FieldRef::new`]
    /// construction time via [`crate::ident::assert_plain_ident`] — it
    /// is safe to `qb.push(*column)` without quoting. The emitter does
    /// not re-validate; the seal is on the constructor.
    Field { column: &'static str },

    /// Scalar literal. Every SQL-bindable type Djogi ships with already
    /// lives in [`FilterValue`]; reusing that enum avoids parallel
    /// variant lists and keeps the emitter's bind path in one place
    /// (it delegates to `push_filter_value`).
    Literal(FilterValue),

    /// `lhs + rhs` — integer or floating-point addition. Typed wrapper
    /// gates this on the sealed [`super::arithmetic::Numeric`] trait.
    Add(Box<ExprNode>, Box<ExprNode>),

    /// `lhs - rhs` — integer or floating-point subtraction. Same sealed
    /// gate as [`ExprNode::Add`].
    Sub(Box<ExprNode>, Box<ExprNode>),

    /// `lhs * rhs` — integer or floating-point multiplication. Same sealed
    /// gate as [`ExprNode::Add`].
    Mul(Box<ExprNode>, Box<ExprNode>),

    /// `lhs / rhs` — integer or floating-point division. Same sealed
    /// gate as [`ExprNode::Add`]. Integer division / division-by-zero
    /// semantics are Postgres's; Djogi does not inject a guard.
    Div(Box<ExprNode>, Box<ExprNode>),

    /// `lhs <op> rhs` — comparison producing an `Expr<bool>` at the
    /// typed-wrapper layer. See [`CmpOp`] for the operator set.
    Cmp {
        op: CmpOp,
        lhs: Box<ExprNode>,
        rhs: Box<ExprNode>,
    },

    /// Aggregate function call — `COUNT(*)` / `COUNT(col)` / `SUM(col)`
    /// / `AVG(col)` / `MIN(col)` / `MAX(col)` with an optional
    /// `FILTER (WHERE ...)` post-filter clause. The typed
    /// [`super::aggregate::AggregateExpr<Out>`] wrapper carries the Rust
    /// return type (`i64` for `COUNT`, `f64` for `AVG`, `V` for
    /// `SUM`/`MIN`/`MAX`) so the emitted scalar decodes to the right
    /// Rust type without runtime casting.
    ///
    /// `arg` is the column or sub-expression being aggregated. For
    /// `COUNT(*)` the dedicated [`AggOp::CountStar`] variant is paired
    /// with an arbitrary `arg` (the emitter ignores it on that branch);
    /// routing `*` through a separate op — rather than a magic
    /// [`ExprNode::Field { column: "*" }`] sentinel — keeps the bare
    /// star away from
    /// [`crate::ident::assert_plain_ident`] / [`crate::ident::debug_assert_ident!`]
    /// and from the column-qualification pass that select_related adds.
    ///
    /// `filter` is an optional boolean sub-expression that gates which
    /// rows contribute to the aggregate. Postgres emits this as
    /// `AGG(arg) FILTER (WHERE <cond>)`. `None` emits the bare aggregate.
    Aggregate {
        /// Which aggregate function to call.
        op: AggOp,
        /// The column or expression being aggregated. Ignored for
        /// [`AggOp::CountStar`] (the emitter renders `COUNT(*)` regardless
        /// of `arg`); the typed [`super::aggregate::AggregateExpr`] surface
        /// stores a placeholder there.
        arg: Box<ExprNode>,
        /// Optional `FILTER (WHERE ...)` clause. `None` emits the bare
        /// aggregate; `Some(cond)` emits
        /// `AGG(arg) FILTER (WHERE <cond>)`.
        filter: Option<Box<ExprNode>>,
        /// Optional explicit Postgres-side cast applied to the aggregate
        /// result. Emits as `AGG(arg)::<cast_to>` before the optional
        /// `FILTER` clause is pushed.
        ///
        /// Why: Postgres widens integer aggregates — `SUM(BIGINT)` returns
        /// `NUMERIC`, `AVG(BIGINT)` returns `NUMERIC`, `SUM(SMALLINT)`
        /// returns `BIGINT`. The typed [`super::aggregate::AggregateExpr`]
        /// surface promises `Out = V` for `SUM` over `V: Numeric` and
        /// `Out = f64` for `AVG`, so the emitter narrows / casts back to
        /// the Rust type sqlx can decode. The cast target is always a
        /// framework-baked `&'static str` from
        /// [`super::aggregate`]'s method bodies — never user input.
        cast_to: Option<&'static str>,
    },
}

/// Aggregate operator — the sub-discriminant inside [`ExprNode::Aggregate`].
///
/// The [`super::aggregate::AggregateExpr<Out>`] wrapper pins the Rust
/// return type per variant (`i64` for `Count`/`CountStar`, `f64` for
/// `Avg`, the column type `V` for `Sum`/`Min`/`Max`). The emitter in
/// [`super::sql::emit_expr`] maps each variant to its SQL keyword and
/// renders `COUNT(*)` specially for [`AggOp::CountStar`] so the bare `*`
/// never flows through the identifier-validation / column-qualification
/// paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AggOp {
    /// `COUNT(col)` — returns `i64`. Counts non-null values of the
    /// argument column.
    Count,
    /// `COUNT(*)` — returns `i64`. Counts every row in the grouping
    /// (or the whole relation on an ungrouped aggregate). The
    /// [`ExprNode::Aggregate::arg`] slot is ignored on this variant;
    /// [`super::aggregate::AggregateExpr`] stores a placeholder there.
    CountStar,
    /// `SUM(col)` — returns `V` (the column's numeric type). Undefined
    /// for non-numeric columns; the typed
    /// [`super::aggregate::AggregateExpr::sum`] constructor gates this
    /// on the sealed [`super::arithmetic::Numeric`] trait.
    Sum,
    /// `AVG(col)` — returns `f64`. Postgres widens integer averages to
    /// `numeric`, but we decode to `f64` for ergonomics; callers who
    /// need `Decimal` precision drop to raw sqlx until a Phase 5
    /// `Decimal`-typed aggregate lands.
    Avg,
    /// `MIN(col)` — returns `V`. Requires the column type to be
    /// Postgres-orderable; the typed surface gates on
    /// sqlx `Type + Decode` rather than Rust `Ord` because `f64`
    /// satisfies the former but not the latter.
    Min,
    /// `MAX(col)` — returns `V`. Same ordering requirement as
    /// [`AggOp::Min`].
    Max,
}

/// Comparison operator — the sub-discriminant inside [`ExprNode::Cmp`].
///
/// Mirrors the subset of [`crate::query::condition::LookupOp`] that takes
/// two expression operands (not, e.g., `IS NULL` which takes one). The
/// emitter maps each variant to the corresponding SQL token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CmpOp {
    /// `lhs = rhs`
    Eq,
    /// `lhs <> rhs`
    Neq,
    /// `lhs > rhs`
    Gt,
    /// `lhs >= rhs`
    Gte,
    /// `lhs < rhs`
    Lt,
    /// `lhs <= rhs`
    Lte,
}
