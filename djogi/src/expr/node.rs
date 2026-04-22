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

use crate::query::condition::{Condition, FilterValue};

// Phase 4 Task 5 landed the `Case` / `Exists` / `Subquery` / `OuterRef`
// variants alongside the `SubqueryNode` payload at the bottom of this
// file. The emitter in `expr::sql` has matching arms in lockstep. The
// `#[non_exhaustive]` marker is crate-private today (same-crate matches
// are already exhaustive); it becomes relevant if/when this enum is
// re-exported.

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
        /// the Rust type the decoder returns. The cast target is always a
        /// framework-baked `&'static str` from
        /// [`super::aggregate`]'s method bodies — never user input.
        cast_to: Option<&'static str>,
    },

    /// `CASE WHEN <cond> THEN <val> [WHEN <cond> THEN <val> ...] ELSE
    /// <default> END` — multi-armed conditional expression.
    ///
    /// The typed builder [`super::case::Case`] / [`super::case::CaseBuilder`]
    /// is the sole construction path. Every arm is a
    /// `(condition, value)` pair — the condition is an arbitrary
    /// [`ExprNode`] that evaluates to boolean (typed as `Expr<bool>` at
    /// the builder surface), the value is the expression whose result
    /// becomes the CASE output when that arm fires. `otherwise` is
    /// **required** (not `Option`) per the Task 5 plan — forcing the
    /// user to decide on the default avoids the silent-NULL footgun
    /// where a CASE with no matching arm produces NULL against a column
    /// the user expected to be non-null.
    Case {
        /// Ordered list of `(condition, value)` pairs. Emitted as
        /// `WHEN <cond> THEN <val>` in vector order; the first arm
        /// whose condition is true wins per Postgres semantics.
        arms: Vec<(ExprNode, ExprNode)>,
        /// `ELSE <default>` expression — evaluated when no arm's
        /// condition is true. Required (no NULL default) per plan
        /// Step 3 decision.
        otherwise: Box<ExprNode>,
    },

    /// `EXISTS (<subquery>)` — boolean-valued subquery predicate.
    ///
    /// The typed surface [`super::subquery::Exists`] owns the construction
    /// path; the wrapped [`SubqueryNode`] carries the correlated
    /// queryset's table + optional `WHERE` payload. `select_column` on
    /// the node is always `None` for this variant — the emitter renders
    /// `SELECT 1` regardless of the wrapped queryset's columns because
    /// EXISTS only cares about row-presence, never scalar values.
    Exists(Box<SubqueryNode>),

    /// Scalar subquery — `(SELECT <col> FROM ... WHERE ... [LIMIT 1])`
    /// usable as any other `Expr<V>` in the outer tree.
    ///
    /// The typed surface [`super::subquery::Subquery<T, V>`] owns the
    /// construction path. `select_column` is always `Some(col)` for
    /// this variant; the emitter renders the stored column verbatim
    /// (already validated at `FieldRef::new` construction time).
    Subquery(Box<SubqueryNode>),

    /// `array_length(column, 1)` — number of elements in a 1-dimensional
    /// Postgres array column.
    ///
    /// The dimension argument is hardcoded to `1`; Djogi arrays are always
    /// 1-dimensional and multi-dimensional arrays are not a supported field
    /// type. Produces an `Expr<i32>` at the typed-wrapper layer; the emitter
    /// renders `array_length({column}, 1)`.
    ///
    /// The `column` string is a `&'static str` validated at
    /// [`crate::query::field::FieldRef::new`] construction time.
    ArrayLength { column: &'static str },

    /// Outer-scope column reference inside a correlated subquery.
    ///
    /// Emits the column name unqualified — Postgres resolves the name
    /// against the enclosing query scope when there is no matching
    /// column in the subquery's own `FROM` list. When both inner and
    /// outer tables expose a same-named column, the unqualified
    /// emission is ambiguous and Postgres raises `42702`. Task 5 ships
    /// the unqualified form; a qualified variant (carrying the outer
    /// table alias) is deferred alongside the broader `parent_table`
    /// threading needed for `select_related + filter_expr` composition.
    /// See the rustdoc on [`super::subquery::OuterRef`] for the caller-
    /// side workaround.
    OuterRef {
        /// The column name in the outer scope. Validated at
        /// [`super::subquery::__macro_support::__make_outer_ref`]
        /// construction time via
        /// [`crate::ident::assert_plain_ident`]; safe to push directly.
        column: &'static str,
    },

    /// A Postgres `INTERVAL` literal derived from a `time::Duration`.
    ///
    /// Emitted as the raw SQL token `INTERVAL '{microseconds} microseconds'` —
    /// no bind parameter is used because `tokio-postgres` / `postgres-types`
    /// does not ship a `ToSql` impl for `time::Duration` (the `time` crate's
    /// native duration type). Microseconds are faithful to `Duration`'s
    /// full sub-millisecond precision.
    ///
    /// The microsecond count is clamped to `i64` range at construction
    /// time (via `time::Duration::whole_microseconds() as i64`); values
    /// outside that range are Postgres `INTERVAL` overflows anyway.
    ///
    /// This variant is produced only by the `impl From<time::Duration> for
    /// Expr<time::Duration>` bridge in [`super::literal`]. User code that
    /// arrives here via `Expr::literal(duration)` has already gone through
    /// the typed `From` impl — no raw `ExprNode::IntervalLiteral {..}`
    /// construction from outside the crate is possible.
    IntervalLiteral {
        /// Microseconds — the full precision of `time::Duration` expressed
        /// as a `BIGINT`-compatible count, emitted as
        /// `INTERVAL '{microseconds} microseconds'`.
        microseconds: i64,
    },
}

/// Internal subquery payload — the untyped counterpart to the typed
/// [`super::subquery::Subquery<T, V>`] / [`super::subquery::Exists`]
/// wrappers.
///
/// Carries the minimum the emitter needs to render
/// `SELECT <col or 1> FROM <table> [WHERE <condition>]`:
///
/// - `table` — always `<T as Model>::table_name()` from the typed
///   surface (a `&'static str`; never user input).
/// - `select_column` — `Some(col)` for scalar subqueries (the typed
///   wrapper pins it via [`crate::query::field::FieldRef`] so the
///   identifier is always validated); `None` for EXISTS, where the
///   emitter renders `SELECT 1`.
/// - `where_clause` — the correlated predicate, stored as a
///   [`Condition`] tree (not lowered to [`ExprNode`]). Reusing the
///   battle-tested [`crate::query::sql::emit_condition`] walk means
///   every `LookupOp` variant the Phase 2 `filter` closure produces
///   composes inside a subquery without parallel emitter code. See the
///   module header on [`super::subquery`] for the rationale behind the
///   "store `Condition` alongside" design decision the plan's Task 5
///   brief laid out.
#[derive(Debug, Clone)]
pub(crate) struct SubqueryNode {
    /// Subquery's `FROM` table — `<T as Model>::table_name()` from the
    /// typed [`super::subquery::Subquery<T, V>`] surface.
    pub(crate) table: &'static str,
    /// Scalar subqueries store `Some(col)`; the EXISTS path stores
    /// `None` and the emitter renders `SELECT 1`.
    pub(crate) select_column: Option<&'static str>,
    /// The subquery's `WHERE` clause — the correlated predicate,
    /// carried verbatim from the typed
    /// [`crate::query::QuerySet<T>`]'s accumulated
    /// [`Condition`] tree. `None` when the typed queryset carries no
    /// filters; the emitter skips the `WHERE` clause on that branch.
    pub(crate) where_clause: Option<Condition>,
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// need `Decimal` precision use `ctx.raw_scalar` until a Phase 5
    /// `Decimal`-typed aggregate lands.
    Avg,
    /// `MIN(col)` — returns `V`. Requires the column type to be
    /// Postgres-orderable; the typed surface gates on
    /// `postgres_types::FromSql` rather than Rust `Ord` because `f64`
    /// satisfies the former but not the latter.
    Min,
    /// `MAX(col)` — returns `V`. Same ordering requirement as
    /// [`AggOp::Min`].
    Max,
    /// `ARRAY_AGG(col)` — collects non-null values into a Postgres array.
    ///
    /// Returns `Vec<V>` at the Rust level. The typed builder on
    /// [`super::aggregate::FieldRef`] pins `Out = Vec<V>` so the
    /// annotate decode path calls `row.try_get::<_, Vec<V>>(alias)`.
    /// postgres-types decodes Postgres arrays into `Vec<V>` when `V`
    /// implements `FromSql` — all scalar column types Djogi ships already
    /// satisfy that bound.
    ArrayAgg,
    /// `JSONB_AGG(col)` — aggregates rows into a JSON array, returned as
    /// `serde_json::Value`.
    ///
    /// Uses `JSONB_AGG` rather than `JSON_AGG` because Djogi standardises
    /// on JSONB for all JSON storage and wire formats (see `docs/spec/decisions.md`).
    JsonAgg,
    /// `STRING_AGG(col, sep)` — concatenates non-null string values with a
    /// separator.
    ///
    /// The separator is stored inline in the variant so the emitter can
    /// push it as a bind parameter without a separate `ExprNode`. Carrying
    /// the separator directly on the variant rather than as a second
    /// `ExprNode` child of `Aggregate { arg, .. }` keeps the existing
    /// `Aggregate` layout unchanged — no other variant needs a second
    /// operand.
    ///
    /// The separator is user-supplied at `.string_agg("sep")` call time
    /// and bound via `acc.push_bind(sep.to_string())` to avoid any risk
    /// of SQL injection from a runtime-computed separator string.
    StringAgg(String),
    /// `BOOL_AND(col)` — true if every non-null value in the column is
    /// true. Requires a boolean column; the typed builder gates on
    /// `V: Into<bool>`.
    BoolAnd,
    /// `BOOL_OR(col)` — true if at least one non-null value in the
    /// column is true. Requires a boolean column; the typed builder gates
    /// on `V: Into<bool>`.
    BoolOr,
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
