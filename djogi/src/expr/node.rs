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
    /// `FILTER (WHERE ...)` post-filter clause and an optional window
    /// (`OVER (...)`) clause. The typed
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
    ///
    /// `distinct` reserves the `DISTINCT` keyword slot for Phase 6.5 T4's
    /// `.distinct()` builder method. Always `false` until T4 wires it.
    ///
    /// `window` is an optional [`super::window::WindowSpec`] that promotes
    /// this aggregate to a window function via `OVER (...)`. Supplied by
    /// the `.over(|w| ...)` method on
    /// [`super::aggregate::AggregateExpr`] (T3). `None` leaves the
    /// aggregate bare; the terminal-layer helpers in
    /// `query::sql` add `OVER ()` for the ungrouped annotate path when
    /// `window` is `None`.
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
        /// When `true`, the `DISTINCT` keyword is emitted before the aggregate
        /// argument: `AGG(DISTINCT col)`. Set via
        /// [`super::aggregate::AggregateExpr::distinct`] (T4). Fetch-time
        /// validation in [`super::sql::check_aggregate_legality`] rejects
        /// combinations that Postgres does not accept or that Djogi's current
        /// IR cannot correctly represent.
        distinct: bool,
        /// Optional user-specified window clause produced by
        /// [`super::aggregate::AggregateExpr::over`]. `None` means the
        /// aggregate has no `OVER` clause of its own; the ungrouped
        /// annotate path in `query::sql` wraps `None`-window aggregates in
        /// `OVER ()` for backwards compatibility. `Some(spec)` emits the
        /// full `OVER (PARTITION BY ... ORDER BY ... frame)` from the spec.
        window: Option<crate::expr::window::WindowSpec>,
        /// Per-aggregate `ORDER BY` clause(s). Set via
        /// [`super::aggregate::AggregateExpr::order_by`]. Empty `Vec`
        /// emits no ORDER BY; non-empty emits
        /// `AGG(arg ORDER BY <ord1>, <ord2>, ...)` (or for STRING_AGG:
        /// `STRING_AGG(arg, sep ORDER BY ...)`).
        ///
        /// Some aggregates' result depends on input order — `ARRAY_AGG`,
        /// `JSONB_AGG`, `STRING_AGG`, plus the ordered-set / hypothetical-
        /// set families (PERCENTILE_CONT, MODE, etc., per the
        /// WITHIN GROUP modifier surface that consumes this slot
        /// indirectly through the adjacent `within_group_order_by`
        /// modifier). Without this slot, callers couldn't get
        /// deterministic results from order-sensitive aggregates without
        /// wrapping the whole query in a derived table.
        ///
        /// Also unblocks `STRING_AGG(DISTINCT col, sep ORDER BY other)` —
        /// Postgres rejects that combination unless an ORDER BY is
        /// supplied; the fetch-time check in
        /// [`super::sql::check_aggregate_legality`] still rejects
        /// `STRING_AGG(DISTINCT ...)` with no ORDER BY but accepts the
        /// combination with one.
        order_by: Vec<crate::query::order::OrderExpr>,
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

    /// `EXTRACT(YEAR FROM CURRENT_DATE)::INTEGER` — the current calendar year
    /// as an `i32`, evaluated server-side per query.
    ///
    /// Produced by [`super::Expr::current_year`]. Composes with the existing
    /// `Expr<i32>` arithmetic IR so e.g.
    /// `Expr::current_year() - f.estimated_birth_year().as_expr()` yields the
    /// row's age as an `Expr<i32>`.
    ///
    /// # SQL emission
    ///
    /// The emitter renders the literal token stream
    /// `EXTRACT(YEAR FROM CURRENT_DATE)::INTEGER` — no bind parameter is
    /// taken because there is no user-supplied value (the year is read from
    /// the Postgres server clock at query time). The explicit `::INTEGER`
    /// cast narrows Postgres's native `numeric` return from `EXTRACT` back
    /// to a Rust-decodable `i32` so the typed surface's `Expr<i32>` promise
    /// holds end-to-end.
    ///
    /// # Volatility note
    ///
    /// `CURRENT_DATE` is `STABLE` (not `IMMUTABLE`) per Postgres semantics —
    /// it changes by calendar day, not within a single transaction. The
    /// IR does not annotate volatility on `ExprNode` variants today; callers
    /// who need a deterministic snapshot of "now" inside a long-running
    /// transaction should bind a literal `i32` instead of using this helper.
    CurrentYear,

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

    /// Table-qualified outer-scope column reference inside a correlated
    /// subquery — emits `<table>.<column>` rather than the unqualified
    /// [`Self::OuterRef`] form.
    ///
    /// Phase 7-Zero-2 T13b introduced this variant for the macro-emitted
    /// M2M EXISTS predicate, where the inner through table and the outer
    /// peer table both carry framework `id` / `created_at` / `updated_at`
    /// columns. The unqualified form would raise `42702 column reference
    /// "id" is ambiguous`; emitting `<peer_table>.id` disambiguates.
    ///
    /// Both fields are `&'static str` validated at construction time via
    /// [`crate::ident::assert_plain_ident`]; safe to push as raw SQL.
    OuterRefColumn {
        /// Outer-scope table name (e.g. `"groups"`). Validated.
        table: &'static str,
        /// Column on that outer table (e.g. `"id"`). Validated.
        column: &'static str,
    },

    /// `<col> @@ to_tsquery('<dictionary>', $n)` — full-text search match.
    ///
    /// Produced by `FtsFieldRef::matches(query)`. The column name is the
    /// GENERATED ALWAYS AS tsvector column (typically `"search"`). The
    /// dictionary name is embedded into the SQL as a literal token (e.g.
    /// `to_tsquery('english', $1)`) because it is a Postgres configuration
    /// name, not a user value — it came from `#[model(fts = { dictionary = "..."
    /// })]` and was validated at macro parse time. The query text is bound
    /// as a parameter.
    ///
    /// The emitter renders: `<column> @@ to_tsquery('<dictionary>', $n)`
    TsMatch {
        /// The tsvector column name (e.g. `"search"`). Validated at
        /// construction via `assert_plain_ident`; safe to push as raw SQL.
        column: &'static str,
        /// Postgres text-search config name (e.g. `"english"`). Validated
        /// at macro parse time; embedded literally into the SQL.
        dictionary: &'static str,
        /// The tsquery text bound as a parameter (e.g. `"planet & earth"`).
        query_text: String,
    },

    /// `ts_rank(<col>, to_tsquery('<dictionary>', $n))` — relevance score.
    ///
    /// Produced by `FtsFieldRef::rank(query)`. Returns an `f32` scalar
    /// that Postgres computes per-row as the document's relevance against
    /// the query. Useful in `ORDER BY ... DESC` to surface the most
    /// relevant results first.
    ///
    /// The emitter renders: `ts_rank(<column>, to_tsquery('<dictionary>', $n))`
    TsRank {
        /// The tsvector column name. Validated at construction.
        column: &'static str,
        /// Dictionary name embedded literally. Validated at macro parse time.
        dictionary: &'static str,
        /// The tsquery text bound as a parameter.
        query_text: String,
    },

    /// `ts_rank_cd(<col>, to_tsquery('<dictionary>', $n))` — cover-density rank.
    ///
    /// Like `TsRank` but uses the cover-density ranking algorithm, which
    /// weighs proximity of matching terms more heavily. Useful when term
    /// position within the document matters.
    TsRankCd {
        /// The tsvector column name.
        column: &'static str,
        /// Dictionary name.
        dictionary: &'static str,
        /// The tsquery text bound as a parameter.
        query_text: String,
    },

    /// A Postgres `INTERVAL` literal derived from a `time::Duration`.
    ///
    /// Emitted as the raw SQL token `INTERVAL '{microseconds} microseconds'` —
    /// no bind parameter is used because `tokio-postgres` / `postgres-types`
    /// does not ship a `ToSql` impl for `time::Duration` (the `time` crate's
    /// native duration type). Microseconds are faithful to `Duration`'s
    /// full sub-millisecond precision.
    ///
    /// The microsecond count is saturating-clamped to `i64` range at
    /// construction time (via [`super::literal::saturating_micros`]);
    /// Durations outside that range encode as `i64::MAX` or `i64::MIN`
    /// (~±292,277 years) rather than wrapping silently. Those extremes are
    /// already Postgres `INTERVAL` overflows, so saturation is the correct
    /// sentinel value.
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

    // ── Spatial expressions (Phase 6 `spatial` feature) ─────────────────────
    /// A spatial predicate or expression, delegating to
    /// [`super::spatial::SpatialExpr`] for SQL emission.
    ///
    /// Gated on `#[cfg(feature = "spatial")]` so builds without the feature
    /// never see any PostGIS references. The variant is only ever constructed
    /// by [`crate::query::field::FieldRef<M, GeoPoint>::within_km`] (produces
    /// `SpatialExpr::Within`, typed as `Expr<bool>`) or captured inside
    /// [`crate::query::order::OrderExpr::SpatialDistance`] (uses
    /// `SpatialExpr::Distance`, typed as `Expr<f64>`).
    ///
    /// `SpatialExpr` is `Clone + Debug`, so this variant does not break the
    /// enum's own `#[derive(Debug, Clone)]`.
    #[cfg(feature = "spatial")]
    Spatial(crate::expr::spatial::SpatialExpr),
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
    /// `BIT_AND(col)` — bitwise AND across all non-null values, returned
    /// as the column's integer type. Postgres defines this for
    /// `SMALLINT`, `INTEGER`, `BIGINT` and bit-string types; Djogi's
    /// typed builder gates on the sealed
    /// [`super::aggregate::IntegerColumn`] trait, which admits the three
    /// integer scalar types only.
    BitAnd,
    /// `BIT_OR(col)` — bitwise OR across all non-null values. Same type
    /// gating as [`AggOp::BitAnd`].
    BitOr,
    /// `BIT_XOR(col)` — bitwise XOR across all non-null values. Same
    /// type gating as [`AggOp::BitAnd`]. PostgreSQL 14+ feature; Djogi's
    /// floor (PostgreSQL 18) safely supports it.
    BitXor,
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
