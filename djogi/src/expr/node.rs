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
        /// Second argument for binary (two-arg) aggregates. `None` for
        /// the unary family (COUNT / SUM / AVG / MIN / MAX / ARRAY_AGG /
        /// JSONB_AGG / STRING_AGG / BOOL_AND / BOOL_OR / EVERY / BIT_*
        /// / STDDEV_* / VAR_* / GROUPING). `Some(node)` for the binary
        /// family (COVAR_POP / COVAR_SAMP / CORR / REGR_* / JSON_OBJECT_AGG
        /// / JSONB_OBJECT_AGG), where the `arg` slot carries the first
        /// column (`y` for stats / `key` for json-object) and `arg2`
        /// carries the second column (`x` for stats / `value` for
        /// json-object).
        ///
        /// Cluster E T5 introduced this slot to back `covar_pop` / `corr`
        /// / `regr_*` / `jsonb_object_agg`. The slot is backward-compatible
        /// — every pre-existing unary-aggregate constructor (`unary_agg`,
        /// the `string_agg` shape) sets `arg2: None`, so the unary
        /// emission path remains untouched. The emitter ignores `arg2`
        /// on the unary family and renders the comma-separated second
        /// arg only when the variant is recognised as binary.
        arg2: Option<Box<ExprNode>>,
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
///
/// # Why `PartialEq` only (no `Eq`)
///
/// [`AggOp::SpatialClusterWithin`] carries an inline `f64` distance,
/// and `f64` has only [`PartialEq`] — NaN is not reflexively equal to
/// itself. The crate uses `matches!` for variant discrimination (which
/// only needs `PartialEq`), so dropping `Eq` is sound; no downstream
/// code keys an `AggOp` into a `HashMap` / `HashSet`.
#[derive(Debug, Clone, PartialEq)]
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
    /// `EVERY(col)` — Postgres-standard alias for `BOOL_AND`. Both produce
    /// identical results; the alias is preserved through the IR so the
    /// emitter renders the keyword the user wrote (matching adopters'
    /// expectations from the `pg` docs / SQL standard). Same boolean column
    /// gating as [`AggOp::BoolAnd`].
    Every,
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
    /// `STDDEV_POP(col)` — population standard deviation, returned as
    /// `f64` (cast to `DOUBLE PRECISION` to honour the typed surface's
    /// `Out = f64` promise across integer and float inputs).
    StddevPop,
    /// `STDDEV_SAMP(col)` — sample standard deviation. Postgres' default
    /// "stddev" is the sample form; both spellings are exposed.
    StddevSamp,
    /// `STDDEV(col)` — Postgres alias for [`AggOp::StddevSamp`]. Carried
    /// as a distinct variant so the emitter preserves the spelling the
    /// caller used (matching the `every` / `bool_and` alias treatment).
    Stddev,
    /// `VAR_POP(col)` — population variance, returned as `f64`.
    VarPop,
    /// `VAR_SAMP(col)` — sample variance.
    VarSamp,
    /// `VARIANCE(col)` — Postgres alias for [`AggOp::VarSamp`].
    Variance,
    /// `COVAR_POP(y, x)` — population covariance, returned as `f64`.
    /// Two-arg aggregate: `arg` carries `y`, `arg2` carries `x`.
    /// `y` first matches Postgres convention (the dependent variable
    /// is the first argument across the regression / covariance family).
    CovarPop,
    /// `COVAR_SAMP(y, x)` — sample covariance, returned as `f64`. Same
    /// arg ordering as [`AggOp::CovarPop`].
    CovarSamp,
    /// `CORR(y, x)` — Pearson correlation coefficient, returned as
    /// `f64`. Same arg ordering as [`AggOp::CovarPop`].
    Corr,
    /// `REGR_AVGX(y, x)` — average of the independent variable across
    /// rows where both columns are non-null. Returned as `f64`.
    RegrAvgx,
    /// `REGR_AVGY(y, x)` — average of the dependent variable across
    /// rows where both columns are non-null. Returned as `f64`.
    RegrAvgy,
    /// `REGR_COUNT(y, x)` — number of input rows where both columns
    /// are non-null. Postgres returns `BIGINT` here (unlike the rest
    /// of the regression family which returns `DOUBLE PRECISION`); the
    /// typed surface returns `AggregateExpr<i64>` accordingly.
    RegrCount,
    /// `REGR_INTERCEPT(y, x)` — y-intercept of the least-squares-fit
    /// line through the (y, x) pairs. Returned as `f64`.
    RegrIntercept,
    /// `REGR_R2(y, x)` — coefficient of determination of the
    /// least-squares-fit line. Returned as `f64`.
    RegrR2,
    /// `REGR_SLOPE(y, x)` — slope of the least-squares-fit line
    /// through the (y, x) pairs. Returned as `f64`.
    RegrSlope,
    /// `REGR_SXX(y, x)` — sum of squares of the independent variable
    /// (sum of `(x - avg(x))^2`). Returned as `f64`.
    RegrSxx,
    /// `REGR_SXY(y, x)` — sum of products of (y, x) deviations
    /// (sum of `(x - avg(x)) * (y - avg(y))`). Returned as `f64`.
    RegrSxy,
    /// `REGR_SYY(y, x)` — sum of squares of the dependent variable
    /// (sum of `(y - avg(y))^2`). Returned as `f64`.
    RegrSyy,
    /// `JSON_OBJECT_AGG(key, value)` — builds a `json` object from
    /// per-row key/value pairs. `arg` carries the key column, `arg2`
    /// carries the value column. Returns `serde_json::Value` at the
    /// typed surface.
    ///
    /// Distinct from [`AggOp::JsonbObjectAgg`] in the Postgres return
    /// type (`json` vs `jsonb`); both are exposed because adopters
    /// needing JSON output (e.g. for an external consumer that cannot
    /// handle JSONB) have no other path today.
    JsonObjectAgg,
    /// `JSONB_OBJECT_AGG(key, value)` — `jsonb` variant of
    /// [`AggOp::JsonObjectAgg`]. Same shape, different Postgres return
    /// type. Djogi standardises on JSONB elsewhere (see
    /// `docs/spec/decisions.md`); this variant is the recommended
    /// default.
    JsonbObjectAgg,
    /// `GROUPING(col)` — Postgres group-set helper that returns `1` if
    /// the column was rolled up in the current result row (i.e. it is
    /// a subtotal row from `ROLLUP` / `CUBE` / `GROUPING SETS`), `0`
    /// otherwise. Returns `INTEGER` at the Postgres level; the typed
    /// surface decodes into `i32`.
    ///
    /// Single-column form only for v0.1.0; the variadic
    /// `GROUPING(c1, c2, …, cN)` form (which returns a bitmask) is a
    /// follow-up task because it needs an N-arg slot and a typed bitmask
    /// return type.
    Grouping,
    /// `ST_Centroid(ST_Collect(<col>))::geography` — per-group centroid
    /// of point geometries. Fused two-call shape (the emitter wraps
    /// `ST_Collect` inside `ST_Centroid` and casts back to geography).
    /// Returns `GeoPoint`. Gated on `feature = "spatial"`.
    ///
    /// Sibling of [`AggOp::ConvexHull`] (which currently lives in the
    /// `SpatialExpr` family for historical reasons; future cleanup will
    /// migrate it here so all PostGIS aggregates inherit the same
    /// modifier composition — `.distinct()` / `.filter()` / `.over()` /
    /// `.order_by()` work uniformly through the `Aggregate` envelope).
    #[cfg(feature = "spatial")]
    SpatialCentroid,
    /// `ST_Collect(<col>)::geography` — per-group multi-geometry
    /// collection. Returns a `MultiPoint` for `GeoPoint` inputs (and
    /// the corresponding multi-shape for other geography inputs once
    /// the typed surface extends to non-point geographies).
    /// Sibling of [`AggOp::SpatialCentroid`].
    #[cfg(feature = "spatial")]
    SpatialCollect,
    /// `ST_Union(<col>::geometry)::geography` — per-group region-merging
    /// aggregate. Folds polygonal inputs into a single combined region.
    /// Returns a `MultiPolygon` — Djogi's typed surface restricts the
    /// receiver to polygon-shaped fields (`Polygon`, `MultiPolygon`) so
    /// the decode is sound; point-shaped inputs use [`AggOp::SpatialCollect`]
    /// (T12's `collect()`) instead. Gated on `feature = "spatial"`.
    #[cfg(feature = "spatial")]
    SpatialUnion,
    /// `ST_Extent(<col>::geometry)::geometry::geography` — per-group 2D
    /// bounding-box aggregate. Postgres returns the special `box2d` type
    /// which Djogi casts through `geometry` (yielding a four-vertex
    /// rectangle Polygon) and back to `geography` for the typed decode.
    /// Returns `Polygon`. Gated on `feature = "spatial"`.
    ///
    /// The `box2d::geometry::geography` cast chain is well-defined
    /// PostGIS — the geometry-side cast produces a polygon footprint,
    /// and the geography-side cast keeps the value on the geography
    /// substrate so adopters get back a `Polygon` they can decompose
    /// with the existing geometry surface.
    #[cfg(feature = "spatial")]
    SpatialExtent,
    /// `ST_3DExtent(<col>::geometry)::geometry::geography` — per-group
    /// 3D bounding-box aggregate. Same cast chain as
    /// [`AggOp::SpatialExtent`] but the underlying Postgres type is
    /// `box3d`; the geometry-side cast projects the 3D box to its 2D
    /// polygon footprint, and the geography-side cast keeps the value
    /// on the geography substrate. Returns `Polygon`.
    /// Gated on `feature = "spatial"`.
    ///
    /// Adopters with true 3D data should reach for `ctx.raw_scalar`
    /// against the `box3d` type directly — Djogi's typed geography
    /// surface is 2D-only.
    #[cfg(feature = "spatial")]
    SpatialExtent3D,
    /// `ST_MakeLine(<col>::geometry)::geography` — per-group
    /// LineString builder. Connects per-row points into a single
    /// LineString in row order, or per-aggregate ORDER BY order when
    /// `.order_by(field)` is chained. Returns `LineString`.
    /// Gated on `feature = "spatial"`.
    ///
    /// Order-sensitive: the resulting line's vertex sequence follows
    /// row order, so this aggregate naturally consumes T1's
    /// `.order_by(field)` modifier — the per-aggregate ORDER BY
    /// clause lands inside the `ST_MakeLine` parens to control
    /// vertex sequence at the aggregate level.
    ///
    /// Sibling [`AggOp::SpatialLineAgg`] (Cluster E T14b) handles the
    /// "collect already-existing LineStrings into a MultiLineString"
    /// use case once the `MultiLineString` geo type lands.
    #[cfg(feature = "spatial")]
    SpatialMakeLine,
    /// `ST_LineAgg(<col>::geometry)::geography` — per-group
    /// `MultiLineString` builder. Collects per-row `LineString` values
    /// into a single `MultiLineString`. Returns `MultiLineString`.
    /// Gated on `feature = "spatial"`.
    ///
    /// `ST_LineAgg` is PostgreSQL 17+ / PostGIS 3.5+; on earlier
    /// installations the equivalent shape is
    /// `ST_LineFromMultiPoint(ST_Collect(<col>))` for a multipoint
    /// input. Djogi targets PG 18 + PostGIS 3.5, so the canonical
    /// `ST_LineAgg` keyword is the safe choice. If a future
    /// installation drift surfaces, the emitter arm is the single
    /// migration site.
    ///
    /// Cluster E T14b retroactively shipped this aggregate after
    /// Track A's initial deferral — no arbitrary deferrals per
    /// `feedback_no_arbitrary_deferrals.md`.
    #[cfg(feature = "spatial")]
    SpatialLineAgg,
    /// `ST_Collect(<col>::geometry)::geography` — per-group polygon
    /// collection (portable fallback for `ST_PolygonAgg`). Returns
    /// `MultiPolygon`. Gated on `feature = "spatial"`.
    ///
    /// `ST_PolygonAgg` is PostGIS 3.5+; Djogi's documented PostGIS
    /// floor is 3.x (see `docs/guide/spatial.md`), so the emitter
    /// uses the portable `ST_Collect` form which produces an
    /// equivalent MultiPolygon for polygon-typed inputs. The
    /// distinct AggOp variant carries the intent (callers asked for
    /// `polygon_agg()` semantics, not the more general `collect()`)
    /// and keeps a clean migration path if Djogi ever raises the
    /// PostGIS floor — only the emitter arm changes.
    #[cfg(feature = "spatial")]
    SpatialPolygonAgg,
    /// `ST_ClusterIntersecting(<col>::geometry)::geography[]` — per-
    /// group clustering aggregate that groups input geometries which
    /// mutually intersect into per-cluster collections. Returns
    /// `geometry[]` at the Postgres level; Djogi casts the array
    /// element type to `geography` so the typed surface decodes into
    /// `Vec<MultiPolygon>`. Gated on `feature = "spatial"`.
    ///
    /// Available on polygon-shaped fields only — the typed return
    /// `Vec<MultiPolygon>` matches the natural PostGIS output for
    /// polygonal inputs. Point-shaped inputs produce `Vec<MultiPoint>`
    /// at the Postgres level which would break the typed decode;
    /// adopters wanting clustering semantics over points reach for
    /// the existing window-function `cluster_by_proximity`.
    #[cfg(feature = "spatial")]
    SpatialClusterIntersecting,
    /// `ST_ClusterWithin(<col>::geometry, $1)::geography[]` — per-
    /// group clustering aggregate that groups input geometries within
    /// `distance` meters of each other. The distance is carried inline
    /// on the variant (matching [`AggOp::StringAgg`]'s separator pattern)
    /// and bound as a parameter at emission. Returns
    /// `Vec<MultiPolygon>`. Gated on `feature = "spatial"`.
    ///
    /// Same receiver-shape gating as
    /// [`AggOp::SpatialClusterIntersecting`] — polygon-shaped fields
    /// only.
    #[cfg(feature = "spatial")]
    SpatialClusterWithin(f64),
    /// `ST_MemUnion(<col>::geometry)::geography` — memory-friendly
    /// pairwise-merge variant of [`AggOp::SpatialUnion`]. Same input/
    /// output shape (both fold polygonal inputs into a single
    /// MultiPolygon by merging shared edges); different algorithm.
    /// `ST_Union` sorts inputs and merges along a shared edge tree;
    /// `ST_MemUnion` runs a pairwise merge that uses bounded working
    /// memory but is slower per-row for moderate input sizes.
    /// Returns `MultiPolygon`. Gated on `feature = "spatial"`.
    ///
    /// Adopters with terabyte-scale polygonal inputs use `mem_union()`;
    /// for moderate group sizes [`AggOp::SpatialUnion`] is faster.
    #[cfg(feature = "spatial")]
    SpatialMemUnion,
    /// `ST_Polygonize(<col>::geometry)::geography` — builds polygons
    /// from a per-group set of LineString segments. PostGIS returns a
    /// GeometryCollection at the geometry level; the geography-substrate
    /// cast lets the typed surface decode it as `MultiPolygon` for the
    /// typical line-segments-to-region case. Gated on
    /// `feature = "spatial"`.
    ///
    /// Only available on `LineString` fields — the input must be a
    /// collection of edges for the polygonization algorithm to produce
    /// sensible output. The receiver-type gate enforces this at the
    /// impl-block level.
    #[cfg(feature = "spatial")]
    SpatialPolygonize,
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
