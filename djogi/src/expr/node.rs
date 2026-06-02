//! Internal expression AST node — the untyped dynamic payload behind the
//! typed [`Expr<T>`](super::Expr) wrapper.
//! # What
//! [`ExprNode`] is an untyped enum tree: leaves are `Field { column }` /
//! `Literal(FilterValue)`, internal nodes are arithmetic, comparison, and
//! (in later phases) subquery / aggregate / CASE forms. The typed
//! [`Expr<T>`](super::Expr) wrapper projects `T`-safety over this dynamic
//! core so user-facing APIs stay typed while the SQL emitter only has one
//! variant tree to walk.
//! # Why separate the typed wrapper from the node?
//! - **One emitter walk.** [`super::sql::emit_expr`] matches this enum
//!   exhaustively. If `ExprNode` were polymorphic in `T`, every emitter
//!   call site would monomorphise per-type; by erasing `T` at the enum
//!   boundary we get one codegen path and one set of tests.
//! - **Arithmetic composition.** `Expr<i32> + Expr<i32>` yields `Expr<i32>`
//!   the typed wrapper enforces the operator is only available for
//!   numeric `T`, but the node it wraps stores a plain `Add(Box<_>, Box<_>)`
//!   regardless of `T`. Same pattern for comparisons (`Expr<T>.eq(Expr<T>)
//! -> Expr<bool>` — the wrapper changes `T` from `T` to `bool`, the node
//!   is a `Cmp { op: Eq, .. }`).
//! - **Phase expansion.** Tasks 4 / 5 add `Case`, `Exists`, `Subquery`,
//!   `Aggregate`, and `OuterRef` variants. Keeping the enum untyped means
//!   those additions don't ripple into every type-parameterised site; only
//!   the emitter and a few typed constructors grow.
//! # Where
//! - [`super::Expr`] — typed wrapper, the public surface.
//! - [`super::sql::emit_expr`] — the matching emitter (one arm per variant).
//! - [`crate::query::condition::Condition::Expr`] — the bridge that promotes
//!   an `Expr<bool>` into the filter tree.

use crate::query::condition::FilterValue;
use std::any::TypeId;

// Landed the `Case` / `Exists` / `Subquery` / `OuterRef`
// variants alongside the `SubqueryNode` payload at the bottom of this
// file. The emitter in `expr::sql` has matching arms in lockstep. The
// `#[non_exhaustive]` marker is crate-private today (same-crate matches
// are already exhaustive); it becomes relevant if/when this enum is
// re-exported.

/// Untyped expression tree. The typed [`super::Expr<T>`] wrapper carries
/// the phantom `T` parameter and projects type safety over the dynamic
/// variants stored here. The SQL emitter walks this enum directly.
/// Marked `#[non_exhaustive]` to leave room for Tasks 4 / 5
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

    /// Raw SQL fragment — 2 carve-out for the
    /// `#[computed(sql = "...")]` field surface.
    /// The fragment is a `&'static str` baked at macro expansion time
    /// from the user-authored SQL expression. The macro does NOT parse
    /// the fragment (per the lens, plan §7 #6 resolved 2026-05-03
    /// shipping a tiny in-house SQL parser introduces a
    /// Rust-vs-Postgres parse-divergence surface that is hard to test
    /// exhaustively); it does run a token-level validation pass
    /// confirming that every `\w+`-shaped token resolves to a declared
    /// struct field on the model and that operators come from an
    /// explicit allowlist. Anything outside the allowlist surfaces as
    /// a span-precise compile error.
    /// Emitter wraps the fragment in outer parens at every emission
    /// site for operator-precedence stability under further
    /// composition (the same pattern as
    /// [`crate::query::condition::Condition::RawSql`]).
    /// Construction goes through
    /// [`crate::expr::Expr::__raw_sql_fragment`], which is
    /// `#[doc(hidden)]` and not part of the user-facing API. The
    /// `__`-prefix matches
    /// [`crate::query::condition::Condition::__from_raw_sql_fragment`]'s
    /// convention so adopters reading the rustdoc cannot mistake it
    /// for a supported surface.
    RawSql(&'static str),

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

    /// `lhs AND rhs` — boolean logical AND.
    And(Box<ExprNode>, Box<ExprNode>),

    /// `lhs OR rhs` — boolean logical OR.
    Or(Box<ExprNode>, Box<ExprNode>),

    /// `NOT expr` — boolean logical NOT.
    Not(Box<ExprNode>),

    /// `lhs <op> rhs` — comparison producing an `Expr<bool>` at the
    /// typed-wrapper layer. See [`CmpOp`] for the operator set.
    Cmp {
        op: CmpOp,
        lhs: Box<ExprNode>,
        rhs: Box<ExprNode>,
    },

    /// `GROUPING(c1, c2, …, cN)` — variadic grouping-set bitmask.
    /// Postgres assigns bit `0` (LSB) to the rightmost argument, so
    /// bit `j` is `1` iff `columns[N-1-j]` was rolled up in the
    /// current row under `GROUP BY ROLLUP` / `CUBE` / `GROUPING SETS`,
    /// else `0`.
    /// The single-column form `GROUPING(col)` continues to use
    /// `ExprNode::Aggregate { op: AggOp::Grouping, ... }` — the variadic
    /// IR layout adds a new variant rather than retrofitting an N-arg
    /// slot onto `Aggregate` because GROUPING is the only aggregate
    /// taking 2+ column args and Postgres rejects every aggregate
    /// modifier on it (DISTINCT / ORDER BY / FILTER / OVER all error).
    /// A dedicated variant keeps the bulk-of-aggregate fields
    /// (`distinct`, `filter`, `window`, `order_by`, `within_group_order_by`)
    /// from polluting a metadata function that cannot accept them.
    /// Each arg is `ExprNode::Field { column }` with `column` validated
    /// at constructor time via `crate::ident::assert_plain_ident`. No
    /// other `ExprNode` shape is accepted at the typed-surface level
    /// the free function `grouping_of` only takes `&[&'static str]`.
    GroupingVariadic {
        /// Non-empty list of column references being flagged. Empty
        /// arg list is rejected at the constructor (Postgres also
        /// rejects `GROUPING()` with 0 args).
        args: Vec<ExprNode>,
    },

    /// Aggregate function call — `COUNT(*)` / `COUNT(col)` / `SUM(col)`
    /// / `AVG(col)` / `MIN(col)` / `MAX(col)` with an optional
    /// `FILTER (WHERE ...)` post-filter clause and an optional window
    /// (`OVER (...)`) clause. The typed
    /// [`super::aggregate::AggregateExpr<Out>`] wrapper carries the Rust
    /// return type (`i64` for `COUNT`, `f64` for `AVG`, `V` for
    /// `SUM`/`MIN`/`MAX`) so the emitted scalar decodes to the right
    /// Rust type without runtime casting.
    /// `arg` is the column or sub-expression being aggregated. For
    /// `COUNT(*)` the dedicated [`AggOp::CountStar`] variant is paired
    /// with an arbitrary `arg` (the emitter ignores it on that branch);
    /// routing `*` through a separate op — rather than a magic
    /// [`ExprNode::Field { column: "*" }`] sentinel — keeps the bare
    /// star away from
    /// [`crate::ident::assert_plain_ident`] / [`crate::ident::debug_assert_ident!`]
    /// and from the column-qualification pass that select_related adds.
    /// `filter` is an optional boolean sub-expression that gates which
    /// rows contribute to the aggregate. Postgres emits this as
    /// `AGG(arg) FILTER (WHERE <cond>)`. `None` emits the bare aggregate.
    /// `distinct` reserves the `DISTINCT` keyword slot for 's
    /// `.distinct` builder method. Always `false` when not set.
    /// `window` is an optional [`super::window::WindowSpec`] that promotes
    /// this aggregate to a window function via `OVER (...)`. Supplied by
    /// the `.over(|w| ...)` method on
    /// [`super::aggregate::AggregateExpr`]. `None` leaves the
    /// aggregate bare; the terminal-layer helpers in `query::sql` add
    /// `OVER ()` for the plain ungrouped annotate path only after the
    /// plain-annotation type-state has proven the aggregate kind is
    /// windowable. Ordered-set, hypothetical-set, and metadata aggregates
    /// are rejected before that synthesized-window path.
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
        /// Second argument slot, backing `covar_pop` / `corr`
        /// / `regr_*` / `jsonb_object_agg`. The slot is backward-compatible
        /// every pre-existing unary-aggregate constructor (`unary_agg`,
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
        /// [`super::aggregate::AggregateExpr::distinct`], which is
        /// exposed only on the `ValueAgg` kind impl block — non-value
        /// aggregate families ([`AggOp::Grouping`], [`AggOp::PercentileCont`]
        /// / [`AggOp::PercentileDisc`] / [`AggOp::Mode`],
        /// [`AggOp::HypotheticalRank`] / [`AggOp::HypotheticalDenseRank`] /
        /// [`AggOp::HypotheticalPercentRank`] / [`AggOp::HypotheticalCumeDist`])
        /// reject `.distinct()` at compile time through the type-state
        /// (#89). Fetch-time validation in
        /// [`super::sql::check_aggregate_legality`] still catches
        /// COUNT(DISTINCT *) and `STRING_AGG(DISTINCT ...)` without
        /// per-aggregate `ORDER BY` because those share the `ValueAgg`
        /// kind with their modifier-eligible siblings.
        distinct: bool,
        /// Optional user-specified window clause produced by
        /// [`super::aggregate::AggregateExpr::over`]. `None` means the
        /// aggregate has no `OVER` clause of its own; the ungrouped
        /// annotate path in `query::sql` wraps `None`-window value aggregates
        /// in `OVER ()` for backwards compatibility. Non-windowable aggregate
        /// kinds are rejected by the plain-annotation type-state before SQL
        /// emission. `Some(spec)` emits the full
        /// `OVER (PARTITION BY ... ORDER BY ... frame)` from the spec.
        window: Option<crate::expr::window::WindowSpec>,
        /// Per-aggregate `ORDER BY` clause(s). Set via
        /// [`super::aggregate::AggregateExpr::order_by`]. Empty `Vec`
        /// emits no ORDER BY; non-empty emits
        /// `AGG(arg ORDER BY <ord1>, <ord2>, ...)` (or for STRING_AGG:
        /// `STRING_AGG(arg, sep ORDER BY ...)`).
        /// Some aggregates' result depends on input order — `ARRAY_AGG`,
        /// `JSONB_AGG`, `STRING_AGG`, plus the ordered-set / hypothetical-
        /// set families (PERCENTILE_CONT, MODE, etc., per the
        /// WITHIN GROUP modifier surface that consumes this slot
        /// indirectly through the adjacent `within_group_order_by`
        /// modifier). Without this slot, callers couldn't get
        /// deterministic results from order-sensitive aggregates without
        /// wrapping the whole query in a derived table.
        /// Also unblocks `STRING_AGG(DISTINCT col, sep ORDER BY other)`
        /// Postgres rejects that combination unless an ORDER BY is
        /// supplied; the fetch-time check in
        /// [`super::sql::check_aggregate_legality`] still rejects
        /// `STRING_AGG(DISTINCT ...)` with no ORDER BY but accepts the
        /// combination with one.
        order_by: Vec<crate::query::order::OrderExpr>,
        /// `WITHIN GROUP (ORDER BY ...)` clause for ordered-set
        /// aggregates ([`AggOp::PercentileCont`] / [`AggOp::PercentileDisc`]
        /// / [`AggOp::Mode`]) and (eventually) hypothetical-set
        /// aggregates. Empty for every other aggregate.
        /// Distinct from the per-aggregate [`order_by`](Self::Aggregate::order_by)
        /// slot — that one renders ORDER BY *inside* the aggregate's
        /// parens (`AGG(arg ORDER BY ...)` for value aggregates like
        /// `array_agg`). This slot renders ORDER BY *after* the parens
        /// in a `WITHIN GROUP` clause:
        /// ```sql
        /// PERCENTILE_CONT($1) WITHIN GROUP (ORDER BY response_ms)
        /// ```
        /// For ordered-set and hypothetical-set aggregates this slot is
        /// **mandatory**. The typed builder methods on `FieldRef` populate it
        /// at construction time from the receiver column (default ASC), and
        /// the [`super::aggregate::AggregateExpr::within_group_order_by`]
        /// modifier overrides it for the rare case where the target should
        /// differ from the constructing column or the direction should be
        /// DESC. Direct-IR construction is the only way to produce an empty
        /// slot for these ops, and debug builds assert against that bypass in
        /// [`super::sql::check_aggregate_legality`].
        /// Ordered-set and hypothetical-set aggregates
        /// (`RANK(args) WITHIN GROUP (ORDER BY ...)`)
        /// use this slot.
        within_group_order_by: Vec<crate::query::order::OrderExpr>,
    },

    /// `CASE WHEN <cond> THEN <val> [WHEN <cond> THEN <val> ...] ELSE
    /// <default> END` — multi-armed conditional expression.
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
    /// The typed surface [`super::subquery::Exists`] owns the construction
    /// path; the wrapped [`SubqueryNode`] carries the correlated
    /// queryset's table + optional `WHERE` payload. `select_column` on
    /// the node is always `None` for this variant — the emitter renders
    /// `SELECT 1` regardless of the wrapped queryset's columns because
    /// EXISTS only cares about row-presence, never scalar values.
    Exists(Box<SubqueryNode>),

    /// Scalar subquery — `(SELECT <col> FROM ... WHERE ... [LIMIT 1])`
    /// usable as any other `Expr<V>` in the outer tree.
    /// The typed surface [`super::subquery::Subquery<T, V>`] owns the
    /// construction path. `select_column` is always `Some(col)` for
    /// this variant; the emitter renders the stored column verbatim
    /// (already validated at `FieldRef::new` construction time).
    Subquery(Box<SubqueryNode>),

    /// `array_length(column, 1)` — number of elements in a 1-dimensional
    /// Postgres array column.
    /// The dimension argument is hardcoded to `1`; Djogi arrays are always
    /// 1-dimensional and multi-dimensional arrays are not a supported field
    /// type. Produces an `Expr<i32>` at the typed-wrapper layer; the emitter
    /// renders `array_length({column}, 1)`.
    /// The `column` string is a `&'static str` validated at
    /// [`crate::query::field::FieldRef::new`] construction time.
    ArrayLength { column: &'static str },

    /// `EXTRACT(YEAR FROM CURRENT_DATE)::INTEGER` — the current calendar year
    /// as an `i32`, evaluated server-side per query.
    /// Produced by [`super::Expr::current_year`]. Composes with the existing
    /// `Expr<i32>` arithmetic IR so e.g.
    /// `Expr::current_year() - f.estimated_birth_year().as_expr()` yields the
    /// row's age as an `Expr<i32>`.
    /// # SQL emission
    /// The emitter renders the literal token stream
    /// `EXTRACT(YEAR FROM CURRENT_DATE)::INTEGER` — no bind parameter is
    /// taken because there is no user-supplied value (the year is read from
    /// the Postgres server clock at query time). The explicit `::INTEGER`
    /// cast narrows Postgres's native `numeric` return from `EXTRACT` back
    /// to a Rust-decodable `i32` so the typed surface's `Expr<i32>` promise
    /// holds end-to-end.
    /// # Volatility note
    /// `CURRENT_DATE` is `STABLE` (not `IMMUTABLE`) per Postgres semantics
    /// it changes by calendar day, not within a single transaction. The
    /// IR does not annotate volatility on `ExprNode` variants today; callers
    /// who need a deterministic snapshot of "now" inside a long-running
    /// transaction should bind a literal `i32` instead of using this helper.
    CurrentYear,

    /// Outer-scope column reference inside a correlated subquery.
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
    /// Introduced this variant for the macro-emitted
    /// M2M EXISTS predicate, where the inner through table and the outer
    /// peer table both carry framework `id` / `created_at` / `updated_at`
    /// columns. The unqualified form would raise `42702 column reference
    /// "id" is ambiguous`; emitting `<peer_table>.id` disambiguates.
    /// Both fields are `&'static str` validated at construction time via
    /// [`crate::ident::assert_plain_ident`]; safe to push as raw SQL.
    OuterRefColumn {
        /// Outer-scope table name (e.g. `"groups"`). Validated.
        table: &'static str,
        /// Column on that outer table (e.g. `"id"`). Validated.
        column: &'static str,
    },

    /// Alias-qualified outer-scope column reference inside a correlated
    /// subquery — emits `<alias>.<column>`. Used by LATERAL joins to
    /// unambiguously reference the outer table by its framework alias.
    OuterRefAlias {
        /// The framework alias for the outer scope (e.g. `"l"`).
        alias: &'static str,
        /// Column on that outer table. Validated.
        column: &'static str,
        /// Source model identity for this outer reference. Used to
        /// enforce that lateral inner expressions only reference the
        /// actual outer model for the current lateral join.
        model_type: TypeId,
        /// Source model diagnostic name (`type_name::<M>()`) for error
        /// messages when lateral scoping validation fails.
        model_name: &'static str,
    },

    /// `<col> @@ to_tsquery('<dictionary>', $n)` — full-text search match.
    /// Produced by `FtsFieldRef::matches(query)`. The column name is the
    /// GENERATED ALWAYS AS tsvector column (typically `"search"`). The
    /// dictionary name is embedded into the SQL as a literal token (e.g.
    /// `to_tsquery('english', $1)`) because it is a Postgres configuration
    /// name, not a user value — it came from `#[model(fts = { dictionary = "..."
    /// })]` and was validated at macro parse time. The query text is bound
    /// as a parameter.
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
    /// Produced by `FtsFieldRef::rank(query)`. Returns an `f32` scalar
    /// that Postgres computes per-row as the document's relevance against
    /// the query. Useful in `ORDER BY ... DESC` to surface the most
    /// relevant results first.
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

    // ── pg_trgm (gated on `trgm` feature) ──────────────────────────────────
    /// `<col> % $pattern` — trigram similarity predicate from the
    /// `pg_trgm` Postgres extension.
    /// The `%` operator returns `true` when the two operands' trigram
    /// similarity meets or exceeds the session GUC
    /// `pg_trgm.similarity_threshold` (Postgres default `0.3`). It is the
    /// indexable strategy member of the `gin_trgm_ops` and `gist_trgm_ops`
    /// opclasses — so the predicate is accelerated by any GIN or GiST
    /// index built with one of those opclasses over the column. The
    /// expression evaluates to `boolean`, so it fits into
    /// [`Condition::Expr(Expr<bool>)`][crate::query::condition::Condition::Expr].
    /// Adopters who need a per-query numeric threshold (instead of the
    /// session GUC) use the score-expression form
    /// [`TrgmSimilarityScore`][Self::TrgmSimilarityScore] composed inside
    /// `filter_expr` via the `Expr<T>` comparison API. That form is
    /// **not** accelerated by `gin_trgm_ops` / `gist_trgm_ops`.
    /// Column name is validated at `FieldRef` construction via
    /// `assert_plain_ident`. The pattern is always bound as a positional
    /// parameter — no user text is interpolated into the SQL fragment.
    /// Emitter renders: `<col> % $n`
    /// Gate: `#[cfg(feature = "trgm")]` — enable with
    /// `djogi = { features = ["trgm"] }`.
    #[cfg(feature = "trgm")]
    TrgmSimilarTo {
        /// The `TEXT` column — safe to push as raw SQL (validated at
        /// `FieldRef` construction via `assert_plain_ident`).
        column: &'static str,
        /// Pattern string — bound as `$n`, never interpolated into SQL.
        pattern: String,
    },

    /// `similarity(col, $pattern)` — trigram similarity score expression.
    /// Evaluates the `pg_trgm` `similarity()` function per row, returning
    /// an `f64` in `[0.0, 1.0]`. Useful as an input to `filter_expr` for
    /// per-query numeric-threshold comparisons
    /// (`expr.gte(Expr::literal(0.3_f64))`). Note that this function-form
    /// comparison is **not** accelerated by `gin_trgm_ops` /
    /// `gist_trgm_ops` — those opclasses target the operator family
    /// (`%`, `<->`, …), not arbitrary `similarity(...)` >= comparisons.
    /// For index-accelerated trgm scans, use
    /// [`TrgmSimilarTo`][Self::TrgmSimilarTo].
    /// Wider `Expr<f64>` integration (using the score in `order_by` or
    /// surfacing it via `annotate`) is a documented framework gap; see
    /// `docs/guide/trgm.md` for the current limitations.
    /// Column name is validated at `FieldRef` construction; pattern is
    /// always bound as a positional parameter.
    /// Emitter renders: `similarity(<col>, $n)`
    /// Gate: `#[cfg(feature = "trgm")]`.
    #[cfg(feature = "trgm")]
    TrgmSimilarityScore {
        /// The `TEXT` column — safe to push as raw SQL.
        column: &'static str,
        /// Pattern string — bound as `$n`, never interpolated.
        pattern: String,
    },

    /// A Postgres `INTERVAL` literal derived from a `time::Duration`.
    /// Emitted as the raw SQL token `INTERVAL '{microseconds} microseconds'`
    /// no bind parameter is used because `tokio-postgres` / `postgres-types`
    /// does not ship a `ToSql` impl for `time::Duration` (the `time` crate's
    /// native duration type). Microseconds are faithful to `Duration`'s
    /// full sub-millisecond precision.
    /// The microsecond count is saturating-clamped to `i64` range at
    /// construction time (via [`super::literal::saturating_micros`]);
    /// Durations outside that range encode as `i64::MAX` or `i64::MIN`
    /// (~±292,277 years) rather than wrapping silently. Those extremes are
    /// already Postgres `INTERVAL` overflows, so saturation is the correct
    /// sentinel value.
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

    // ── Spatial expressions (`spatial` feature) ─────────────────────
    /// A spatial predicate or expression, delegating to
    /// [`super::spatial::SpatialExpr`] for SQL emission.
    /// Gated on `#[cfg(feature = "spatial")]` so builds without the feature
    /// never see any PostGIS references. The variant is only ever constructed
    /// by [`crate::query::field::FieldRef<M, GeoPoint>::within_km`] (produces
    /// `SpatialExpr::Within`, typed as `Expr<bool>`) or captured inside
    /// [`crate::query::order::OrderExpr::SpatialDistance`] (uses
    /// `SpatialExpr::Distance`, typed as `Expr<f64>`).
    /// `SpatialExpr` is `Clone + Debug`, so this variant does not break the
    /// enum's own `#[derive(Debug, Clone)]`.
    #[cfg(feature = "spatial")]
    Spatial(crate::expr::spatial::SpatialExpr),

    // ── Row-shape aggregate (#92) ─────────────────────────────────
    /// Row-shape aggregate function — folds a **set of rows** into a single
    /// scalar value (binary `bytea` for v0.1.0 — `ST_AsMVT` / `ST_AsGeobuf`).
    /// # Why a sibling variant, not an extension of [`Self::Aggregate`]
    /// Column aggregates take a single column expression (`AGG(col)`) and
    /// honour a modifier set (`.distinct()`, `.filter(...)`, `.over(...)`,
    /// `.order_by(...)`, `.within_group_order_by(...)`). Row aggregates take
    /// **the entire row** (`AGG(record, ...)`) and reject every modifier in
    /// that set — Postgres would error if you tried `ST_AsMVT(DISTINCT t)` or
    /// `ST_AsGeobuf(t ORDER BY ...)`. Sharing the [`Self::Aggregate`] variant
    /// would silently expose those slots through the [`super::aggregate::AggregateExpr`]
    /// modifier impl blocks; a dedicated variant keeps the kind discipline
    /// crisp.
    /// # SQL shape
    /// The emitter renders `<KEYWORD>(<row_alias>, <bind>, <bind>, …)`
    /// against a caller-supplied row alias (`__djogi_row` by convention).
    /// The outer terminal wraps the inner SELECT in a derived table whose
    /// alias matches this convention so the aggregate's record argument
    /// resolves to the full row record:
    /// ```sql
    /// SELECT ST_AsMVT(__djogi_row, $1, $2, $3, $4)
    /// FROM (
    ///     SELECT t.col1, t.col2, …, <annotations…>
    ///     FROM <table> AS t [WHERE …] [ORDER BY …] [LIMIT …]
    /// ) AS __djogi_row
    /// ```
    /// # Construction
    /// Construction is sealed behind the typed
    /// [`super::row_aggregate::RowAggregate`] wrapper; the `columns` slot is
    /// informational and reserved for future row-shape variants that need
    /// per-column projection metadata. v0.1.0 always emits the entire row
    /// alias, so the emitter does not currently inspect `columns`.
    #[cfg(feature = "spatial")]
    RowAggregate {
        /// Which row-shape aggregate function to emit (and its bindable
        /// argument payload — layer name, extent, geom column name, etc.).
        op: RowAggOp,
        /// Columns from the inner SELECT this row aggregate inspects.
        /// Reserved for future per-column-projection variants — `ST_AsMVT`
        /// / `ST_AsGeobuf` resolve column references from the record type
        /// at Postgres-runtime, so v0.1.0's emitter does not consume this
        /// slot. Kept on the variant so that adding a column-projecting
        /// row aggregate does not force an IR rev.
        /// `#[allow(dead_code)]` because the field is deliberately
        /// reserved — it is populated by every typed constructor (e.g.
        /// `crate::query::row_aggregate_terminal` pulls `T::COLUMNS`
        /// through `inner_columns_for`) so debug diagnostics can inspect
        /// the projection shape even though the emitter currently does
        /// not.
        #[allow(dead_code)]
        columns: Vec<&'static str>,
    },
}

/// Internal subquery payload — the untyped counterpart to the typed
/// [`super::subquery::Subquery<T, V>`] / [`super::subquery::Exists`]
/// wrappers.
/// Carries the minimum the emitter needs to render
/// `SELECT <col or 1> FROM <table> [WHERE <condition>]`:
/// - `table` — always `<T as Model>::table_name()` from the typed
///   surface (a `&'static str`; never user input).
/// - `select_column` — `Some(col)` for scalar subqueries (the typed
///   wrapper pins it via [`crate::query::field::FieldRef`] so the
///   identifier is always validated); `None` for EXISTS, where the
///   emitter renders `SELECT 1`.
/// - `where_clause` — the correlated predicate, stored as a
///   type-erased [`ErasedSubqueryPredicate`] handle.
///   replaced the pre-flip `Option<Condition>` storage so expression
///   subqueries carry full `Q<T>` predicates — including portable root
///   field leaves — without round-tripping through `q_to_condition`.
///   The handle owns a `Q<T>` for some concrete `T: Model`; the
///   trait-object dispatch hides the model parameter so `SubqueryNode`
///   stays type-erased.
#[derive(Debug, Clone)]
pub(crate) struct SubqueryNode {
    /// Subquery's `FROM` table — `<T as Model>::table_name()` from the
    /// typed [`super::subquery::Subquery<T, V>`] surface.
    pub(crate) table: &'static str,
    /// Scalar subqueries store `Some(col)`; the EXISTS path stores
    /// `None` and the emitter renders `SELECT 1`.
    pub(crate) select_column: Option<&'static str>,
    /// The subquery's `WHERE` clause — the correlated predicate, stored
    /// as a type-erased emitter handle. `None` when the typed queryset
    /// carries no filters; the emitter skips the `WHERE` clause on that
    /// branch.
    pub(crate) where_clause: Option<ErasedSubqueryPredicate>,
}

/// Type-erased subquery predicate handle. Wraps an
/// `Arc<dyn SubqueryPredicateEmitter>` so a `SubqueryNode` can carry the
/// outer queryset's `Q<T>` predicate without leaking the model parameter
/// into the type-erased subquery wrappers.
/// The storage rewrite that retires the pre-flip
/// `Option<Condition>` payload. Construction goes through
/// [`Self::from_q`]; every Djogi-trusted predicate emitted from a
/// subquery flows through that path so cache/refresh boundaries can audit
/// (PR4) the trusted-portable provenance carried inside the wrapped
/// `Q<T>`.
/// The trait is `Send + Sync + 'static` because `ExprNode` itself is and
/// `Expr<bool>` is freely cloneable across thread boundaries.
#[derive(Clone)]
pub(crate) struct ErasedSubqueryPredicate(std::sync::Arc<dyn SubqueryPredicateEmitter>);

/// Internal trait the `ErasedSubqueryPredicate` dispatches through. Each
/// concrete implementer carries a `Q<T>` for one specific `T: Model`
/// and emits SQL through the direct walker
/// (`crate::query::sql::emit_q::<T>`).
/// Subquery emission threads the caller's `SqlEmitContext`. This keeps
/// lateral-scope metadata alive for nested expression/subquery emission
/// while preserving the existing column-qualification behavior: subquery
/// table qualification still flows through the IR nodes (`OuterRef`,
/// `OuterRefColumn`, `OuterRefAlias`) and regular root columns continue
/// to emit according to the caller's context.
pub(crate) trait SubqueryPredicateEmitter: Send + Sync + 'static {
    /// Emit the subquery's `WHERE` body into the outer accumulator. The
    /// outer accumulator owns global bind numbering, so binds emitted
    /// here interleave correctly with the surrounding SQL.
    fn emit(
        &self,
        acc: &mut crate::pg::accumulator::SqlAccumulator,
        ctx: crate::query::portable::SqlEmitContext,
    ) -> Result<(), crate::query::portable::PortablePredicateError>;
    /// Format the wrapped `Q<T>` for `Debug` output without requiring
    /// `T: Debug`. Delegates to `Q<T>`'s manual `Debug` impl, which
    /// itself never reaches into model-parameter clone/debug bounds.
    fn fmt_debug(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result;
}

/// Concrete implementer of [`SubqueryPredicateEmitter`]. Wraps a
/// `Q<T>` for some `T: Model`. The model parameter never leaks to the
/// surrounding `SubqueryNode` because the trait object hides it.
struct TypedSubqueryPredicate<T: crate::model::Model> {
    q: crate::query::q::Q<T>,
}

impl<T: crate::model::Model> SubqueryPredicateEmitter for TypedSubqueryPredicate<T> {
    fn emit(
        &self,
        acc: &mut crate::pg::accumulator::SqlAccumulator,
        ctx: crate::query::portable::SqlEmitContext,
    ) -> Result<(), crate::query::portable::PortablePredicateError> {
        crate::query::sql::emit_q::<T>(acc, &self.q, ctx)
    }

    fn fmt_debug(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("TypedSubqueryPredicate")
            .field(&self.q)
            .finish()
    }
}

impl ErasedSubqueryPredicate {
    /// Build an erased predicate handle from a typed `Q<T>`. The handle
    /// owns the predicate; cloning the `ExprNode` clones the underlying
    /// `Arc`, so a single `SubqueryNode` may be emitted any number of
    /// times without re-allocating the predicate.
    pub(crate) fn from_q<T: crate::model::Model>(q: crate::query::q::Q<T>) -> Self {
        Self(std::sync::Arc::new(TypedSubqueryPredicate::<T> { q }))
    }

    /// Emit this subquery predicate into the outer accumulator.
    pub(crate) fn emit(
        &self,
        acc: &mut crate::pg::accumulator::SqlAccumulator,
        ctx: crate::query::portable::SqlEmitContext,
    ) -> Result<(), crate::query::portable::PortablePredicateError> {
        self.0.emit(acc, ctx)
    }
}

impl std::fmt::Debug for ErasedSubqueryPredicate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt_debug(f)
    }
}

/// Aggregate operator — the sub-discriminant inside [`ExprNode::Aggregate`].
/// The [`super::aggregate::AggregateExpr<Out>`] wrapper pins the Rust
/// return type per variant (`i64` for `Count`/`CountStar`, `f64` for
/// `Avg`, the column type `V` for `Sum`/`Min`/`Max`). The emitter in
/// [`super::sql::emit_expr`] maps each variant to its SQL keyword and
/// renders `COUNT(*)` specially for [`AggOp::CountStar`] so the bare `*`
/// never flows through the identifier-validation / column-qualification
/// paths.
/// # Why `PartialEq` only (no `Eq`)
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
    /// need `Decimal` precision use `ctx.raw_scalar` until a
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
    /// Returns `Vec<V>` at the Rust level. The typed builder on
    /// [`super::aggregate::FieldRef`] pins `Out = Vec<V>` so the
    /// annotate decode path calls `row.try_get::<_, Vec<V>>(alias)`.
    /// postgres-types decodes Postgres arrays into `Vec<V>` when `V`
    /// implements `FromSql` — all scalar column types Djogi ships already
    /// satisfy that bound.
    ArrayAgg,
    /// `JSONB_AGG(col)` — aggregates rows into a JSON array, returned as
    /// `serde_json::Value`.
    /// Uses `JSONB_AGG` rather than `JSON_AGG` because Djogi standardises
    /// on JSONB for all JSON storage and wire formats (see `docs/spec/decisions.md`).
    JsonAgg,
    /// `STRING_AGG(col, sep)` — concatenates non-null string values with a
    /// separator.
    /// The separator is stored inline in the variant so the emitter can
    /// push it as a bind parameter without a separate `ExprNode`. Carrying
    /// the separator directly on the variant rather than as a second
    /// `ExprNode` child of `Aggregate { arg, .. }` keeps the existing
    /// `Aggregate` layout unchanged — no other variant needs a second
    /// operand.
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
    /// Single-column form. The variadic `GROUPING(c1, …, cN)` ships
    /// through the dedicated [`ExprNode::GroupingVariadic`] variant
    /// rather than reshaping this op's arg slot.
    Grouping,
    /// `PERCENTILE_CONT(p) WITHIN GROUP (ORDER BY col)` — continuous
    /// percentile (interpolating between adjacent values when the
    /// requested fraction falls between two rows). Returns
    /// `DOUBLE PRECISION` (cast to `f64` at the typed surface).
    /// Ordered-set aggregate — the fraction `p` lives in the `arg` slot
    /// as a literal `f64`; the column being percentiled lives in the
    /// `within_group_order_by` slot. WITHIN GROUP is mandatory and is
    /// populated by the typed ordered-set constructor. DISTINCT and the
    /// per-aggregate `order_by` slot are not exposed on
    /// `AggregateExpr<Out, OrderedSetAgg>`; debug builds assert the same
    /// invariants for direct-IR construction.
    PercentileCont,
    /// `PERCENTILE_DISC(p) WITHIN GROUP (ORDER BY col)` — discrete
    /// percentile (returns the actual value at the percentile cut, no
    /// interpolation). Return type matches the column type — the typed
    /// `AggregateExpr<V>` carries `V = column type`.
    /// Same shape as [`AggOp::PercentileCont`] otherwise — fraction in
    /// `arg`, column in `within_group_order_by`.
    PercentileDisc,
    /// `MODE() WITHIN GROUP (ORDER BY col)` — most common value in the
    /// group. Returns the column type (typed `AggregateExpr<V>` with
    /// `V = column type`).
    /// `MODE()` takes no function arguments; the `arg` slot stores a
    /// sentinel `Field { column: \"\" }` placeholder which the emitter
    /// ignores on this branch (parallel to how `CountStar` ignores
    /// `arg`). The column being mode-aggregated lives in
    /// `within_group_order_by`.
    Mode,
    /// `RANK(value) WITHIN GROUP (ORDER BY col)` — hypothetical-set
    /// rank: the rank that `value` would have if inserted into the
    /// sorted column. Returns `BIGINT` (typed as `i64`).
    /// Distinct from the window-only rank function in
    /// [`super::window_fn::Rank`]: that one ranks each row within a
    /// PARTITION/ORDER window, while this one answers "what rank
    /// would this hypothetical value have?" given a single ordered
    /// set. Same modifier discipline as the ordered-set aggregates
    /// no DISTINCT, no in-paren ORDER BY, mandatory WITHIN GROUP.
    HypotheticalRank,
    /// `DENSE_RANK(value) WITHIN GROUP (ORDER BY col)` — hypothetical-
    /// set dense rank (no gaps in rank numbering when ties occur).
    /// Returns `BIGINT`.
    HypotheticalDenseRank,
    /// `PERCENT_RANK(value) WITHIN GROUP (ORDER BY col)`
    /// hypothetical-set percent rank: the position the hypothetical
    /// value would have as a fraction in `[0.0, 1.0]`. Returns
    /// `DOUBLE PRECISION`.
    HypotheticalPercentRank,
    /// `CUME_DIST(value) WITHIN GROUP (ORDER BY col)` — hypothetical-
    /// set cumulative distribution: the fraction of rows that would
    /// rank at or below the hypothetical value. Returns
    /// `DOUBLE PRECISION`.
    HypotheticalCumeDist,
    /// `ST_Centroid(ST_Collect(<col>))::geography` — per-group centroid
    /// of point geometries. Fused two-call shape (the emitter wraps
    /// `ST_Collect` inside `ST_Centroid` and casts back to geography).
    /// Returns `GeoPoint`. Gated on `feature = "spatial"`.
    /// Sibling of [`AggOp::SpatialConvexHull`] — same wrapped emission
    /// shape (`WRAP(ST_Collect(...))::geography`) and same modifier
    /// composition story.
    #[cfg(feature = "spatial")]
    SpatialCentroid,
    /// `ST_ConvexHull(ST_Collect(<col>::geometry))::geography`
    /// per-group convex-hull aggregate. Migrated from
    /// `ExprNode::Spatial(SpatialExpr::ConvexHull{..})`: the old IR shape
    /// silently dropped `.distinct` / `.filter` / `.over` /
    /// `.order_by` modifiers because those mutate `ExprNode::Aggregate`
    /// only. As a proper AggOp variant ConvexHull now composes uniformly
    /// with the rest of the aggregate family.
    /// Wrapped emission: ST_Collect is the actual aggregate;
    /// ST_ConvexHull is a scalar wrapper applied to the collected
    /// geometry set per group. Same shape as
    /// [`AggOp::SpatialCentroid`].
    #[cfg(feature = "spatial")]
    SpatialConvexHull,
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
    /// (`collect`) instead. Gated on `feature = "spatial"`.
    #[cfg(feature = "spatial")]
    SpatialUnion,
    /// `ST_Extent(<col>::geometry)::geometry::geography` — per-group 2D
    /// bounding-box aggregate. Postgres returns the special `box2d` type
    /// which Djogi casts through `geometry` (yielding a four-vertex
    /// rectangle Polygon) and back to `geography` for the typed decode.
    /// Returns `Polygon`. Gated on `feature = "spatial"`.
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
    /// Order-sensitive: the resulting line's vertex sequence follows
    /// row order, so this aggregate naturally consumes the
    /// `.order_by(field)` modifier — the per-aggregate ORDER BY
    /// clause lands inside the `ST_MakeLine` parens to control
    /// vertex sequence at the aggregate level.
    /// Sibling [`AggOp::SpatialLineAgg`] handles the
    /// "collect already-existing LineStrings into a MultiLineString"
    /// use case once the `MultiLineString` geo type lands.
    #[cfg(feature = "spatial")]
    SpatialMakeLine,
    /// `ST_LineAgg(<col>::geometry)::geography` — per-group
    /// `MultiLineString` builder. Collects per-row `LineString` values
    /// into a single `MultiLineString`. Returns `MultiLineString`.
    /// Gated on `feature = "spatial"`.
    /// `ST_LineAgg` is PostgreSQL 17+ / PostGIS 3.5+; on earlier
    /// installations the equivalent shape is
    /// `ST_LineFromMultiPoint(ST_Collect(<col>))` for a multipoint
    /// input. Djogi targets PG 18 + PostGIS 3.5, so the canonical
    /// `ST_LineAgg` keyword is the safe choice. If a future
    /// installation drift surfaces, the emitter arm is the single
    /// migration site.
    /// This aggregate shipped after the initial deferral — no arbitrary deferrals per
    /// `feedback_no_arbitrary_deferrals.md`.
    #[cfg(feature = "spatial")]
    SpatialLineAgg,
    /// `ST_Collect(<col>::geometry)::geography` — per-group polygon
    /// collection (portable fallback for `ST_PolygonAgg`). Returns
    /// `MultiPolygon`. Gated on `feature = "spatial"`.
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
    /// Only available on `LineString` fields — the input must be a
    /// collection of edges for the polygonization algorithm to produce
    /// sensible output. The receiver-type gate enforces this at the
    /// impl-block level.
    #[cfg(feature = "spatial")]
    SpatialPolygonize,
}

/// Row-shape aggregate operator — the sub-discriminant inside
/// [`ExprNode::RowAggregate`]. Each variant pins the SQL keyword the emitter
/// renders plus the bindable argument payload that variant's PostGIS function
/// accepts.
/// Both variants are PostGIS-only and therefore gated on `feature = "spatial"`.
/// The enum itself is `pub(crate)` — construction is sealed behind the typed
/// [`super::row_aggregate::RowAggregate`] wrapper, which the
/// [`crate::query::annotate::AnnotatedQuerySet`] /
/// [`crate::query::queryset::QuerySet`] terminal builders use to keep adopter
/// code from fabricating row aggregates with hostile bind payloads.
/// # Why `PartialEq` only (no `Eq`)
/// Parallel rationale to [`AggOp`]: variants carry `String` payloads (cheap
/// to compare for equality) but the enum has no requirement to key into a
/// `HashMap` / `HashSet`. Mirroring [`AggOp`]'s `PartialEq`-only stance keeps
/// the two enums shape-compatible for shared emitter helpers.
#[cfg(feature = "spatial")]
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RowAggOp {
    /// `ST_AsMVT(row, layer_name, extent, geom_name, feature_id_name)`
    /// Mapbox Vector Tile encoder. Returns `bytea`; the typed surface
    /// decodes into `Vec<u8>`.
    /// All four trailing arguments are bound through the SQL accumulator so
    /// a runtime-computed layer name cannot SQL-inject into the emission
    /// stream. PostGIS defaults are `'default'` / `4096` / `'geom'` /
    /// `NULL`; the typed surface always emits the layer name + extent +
    /// geom_name explicitly (no SQL-side default reliance) and emits the
    /// feature_id argument only when set so the call falls through to the
    /// Postgres-side `NULL` default when omitted.
    AsMvt {
        /// MVT layer name — appears in the encoded protobuf as the layer
        /// identifier. Always bound as `text`, never spliced as SQL.
        layer_name: String,
        /// Tile extent in MVT coordinate units. PostGIS default is `4096`;
        /// the typed surface caps the value at `i32` because PostGIS's
        /// `ST_AsMVT` signature takes `integer`.
        extent: i32,
        /// Name of the geometry column in the inner row that PostGIS
        /// should treat as the feature geometry. The column must exist in
        /// the inner SELECT and decode as PostGIS `geometry` / `geography`.
        geom_name: String,
        /// Optional name of the column to encode as the MVT feature id.
        /// `None` falls through to PostGIS's `NULL` default (no feature id
        /// features are rendered anonymously).
        feature_id_name: Option<String>,
    },
    /// `ST_AsGeobuf(row, geom_name)` — Geobuf encoder. Returns `bytea`; the
    /// typed surface decodes into `Vec<u8>`.
    /// PostGIS's Geobuf surface accepts a single trailing geometry-column
    /// argument; v0.1.0's typed surface always emits it explicitly so the
    /// emission shape stays stable across PostGIS releases that might
    /// rev the default.
    AsGeobuf {
        /// Name of the geometry column in the inner row. Same role as
        /// [`Self::AsMvt::geom_name`].
        geom_name: String,
    },
}

/// Comparison operator — the sub-discriminant inside [`ExprNode::Cmp`].
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
    /// `lhs IS DISTINCT FROM rhs`
    IsDistinctFrom,
    /// `lhs IS NOT DISTINCT FROM rhs`
    IsNotDistinctFrom,
}
