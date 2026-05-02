//! Expression-IR → SQL emitter.
//!
//! # What
//!
//! [`emit_expr`] walks an [`ExprNode`] tree and pushes the matching SQL
//! tokens + bind parameters onto a [`crate::pg::accumulator::SqlAccumulator`].
//! The entry point is called exactly once per `Condition::Expr` variant
//! by [`crate::query::sql::emit_condition`]; it recurses into itself for
//! nested arithmetic / comparison sub-trees.
//!
//! # Why a separate emitter?
//!
//! [`crate::query::sql::emit_leaf`] handles `column op literal` — the
//! left side is always a bare column name, the right side always a
//! literal. The expression IR generalises both sides: either can be a
//! column, a literal, or a nested arithmetic expression. A recursive
//! emitter is the natural fit; factoring it into its own function
//! keeps the leaf path (with its `parent_table` qualification + `ILIKE`
//! escape rules) un-entangled from the recursive walk.
//!
//! # Column references vs bind parameters
//!
//! - [`ExprNode::Field { column }`] — `acc.push_sql(*column)`. The column
//!   name is a `&'static str` validated at
//!   [`crate::query::field::FieldRef::new`] construction time against
//!   [`crate::ident::assert_plain_ident`]; no re-validation here.
//! - [`ExprNode::Literal(v)`] — delegates to
//!   [`crate::query::sql::push_filter_value`], which calls
//!   `acc.push_bind(v)` for every scalar variant. All user-supplied
//!   values flow through bind parameters; no string interpolation of
//!   user data.
//!
//! # `parent_table` qualification
//!
//! [`crate::query::sql::emit_condition`] takes a
//! `parent_table: Option<&'static str>` argument so `select_related`
//! joined queries qualify bare column references as `{table}.{col}`.
//! The expression IR does **not** carry that argument: its scope is
//! single-table expressions (field-vs-field, arithmetic, literal),
//! and joined expressions would need a separate design pass that
//! answers ownership questions (which table owns `OuterRef { column }`?
//! which child table sources an aggregate over a joined collection?).
//!
//! Concretely: `Condition::Expr` inside a `select_related` filter
//! emits a bare column reference, which Postgres flags as ambiguous if
//! the child contributes a same-named column. Stay on the basic
//! `filter` closure when combining with `select_related`; `filter_expr`
//! is aimed at non-joined predicates.

use crate::expr::node::{AggOp, CmpOp, ExprNode, SubqueryNode};
use crate::pg::accumulator::SqlAccumulator;

/// Check whether an [`ExprNode`] tree contains any aggregate whose `DISTINCT`
/// modifier combination Postgres would reject or that Djogi cannot currently
/// emit correctly.
///
/// # Rejected combinations
///
/// - `COUNT(*)` with `distinct = true` — `COUNT(DISTINCT *)` is not valid SQL.
/// - `STRING_AGG(col, sep)` with `distinct = true` — Postgres requires an
///   explicit per-aggregate `ORDER BY` clause when DISTINCT is combined with
///   `STRING_AGG`. The current IR does not track per-aggregate ORDER BY;
///   this restriction may be lifted in a future release.
///
/// All other `(op, distinct = true)` combinations are accepted and emitted as
/// `AGG(DISTINCT col)`.
///
/// # When to call
///
/// Terminal methods (`fetch_one`, `fetch_all`) call this before building the
/// SQL string so the caller gets a typed `DjogiError::UnsupportedAggregate`
/// rather than a cryptic Postgres syntax error.
pub(crate) fn check_aggregate_legality(node: &ExprNode) -> Result<(), crate::DjogiError> {
    match node {
        ExprNode::Aggregate {
            op,
            distinct,
            arg,
            filter,
            order_by,
            ..
        } => {
            if *distinct {
                match op {
                    AggOp::CountStar => {
                        return Err(crate::DjogiError::UnsupportedAggregate {
                            op: "COUNT(*)",
                            reason: "COUNT(DISTINCT *) is not valid SQL — \
                                     use COUNT(DISTINCT col) via FieldRef::count() instead",
                        });
                    }
                    AggOp::StringAgg(_) if order_by.is_empty() => {
                        // `STRING_AGG(DISTINCT col, sep)` requires a per-
                        // aggregate `ORDER BY` to disambiguate the output
                        // tail. With Cluster E T1, callers can chain
                        // `.order_by(f.other.asc())`; until they do, the
                        // combination is still ill-formed Postgres.
                        return Err(crate::DjogiError::UnsupportedAggregate {
                            op: "STRING_AGG",
                            reason: "STRING_AGG(DISTINCT col, sep) requires a per-aggregate \
                                     ORDER BY clause — chain `.order_by(other_field.asc())` \
                                     to disambiguate, otherwise Postgres rejects the call",
                        });
                    }
                    _ => {}
                }
            }
            // Recurse into arg and filter sub-trees in case there are nested
            // aggregates (unusual but structurally possible).
            check_aggregate_legality(arg)?;
            if let Some(f) = filter {
                check_aggregate_legality(f)?;
            }
            Ok(())
        }
        // Recurse into compound expression nodes.
        ExprNode::Add(l, r) | ExprNode::Sub(l, r) | ExprNode::Mul(l, r) | ExprNode::Div(l, r) => {
            check_aggregate_legality(l)?;
            check_aggregate_legality(r)
        }
        ExprNode::Cmp { lhs, rhs, .. } => {
            check_aggregate_legality(lhs)?;
            check_aggregate_legality(rhs)
        }
        ExprNode::Case { arms, otherwise } => {
            for (cond, val) in arms {
                check_aggregate_legality(cond)?;
                check_aggregate_legality(val)?;
            }
            check_aggregate_legality(otherwise)
        }
        // Leaf nodes and variants with no sub-expressions are trivially valid.
        _ => Ok(()),
    }
}

/// Walk an [`ExprNode`] and push the corresponding SQL fragment onto
/// `acc`. Leaves consume bind slots (via
/// [`crate::query::sql::push_filter_value`]); internal nodes push SQL
/// operator tokens and recurse.
///
/// # Invariants
///
/// - The input tree is always constructed via the typed [`Expr<T>`]
///   surface — user code cannot fabricate an `ExprNode` directly (the
///   enum is `pub(crate)`). That means every arm's operand types match
///   the operator's SQL semantics: `Cmp` operands are always
///   compatible-typed, `Add`/`Sub`/`Mul`/`Div` operands are always
///   [`super::arithmetic::Numeric`]. The emitter does not re-check;
///   the sealed trait + phantom-typed wrapper is the seal.
/// - `ExprNode::Field { column }`'s column string is always a
///   validated identifier (see
///   [`crate::query::field::FieldRef::new`]); safe to `acc.push_sql(*column)`.
pub(crate) fn emit_expr(acc: &mut SqlAccumulator, node: &ExprNode) {
    match node {
        ExprNode::Field { column } => {
            // Bare column reference — validated at FieldRef construction.
            acc.push_sql(column);
        }
        ExprNode::Literal(v) => {
            // `push_filter_value` consumes the value, so clone it — the
            // expression tree may be emitted more than once if, e.g., a
            // retry path re-emits the same `UpdateStmt`. All scalar
            // variants of `FilterValue` are cheap to clone (most are
            // `Copy` or carry a short `String`); the boxed `List`/`Pair`
            // shapes never reach here because `ExprNode::Literal` only
            // carries scalar values from the `impl From<V> for Expr<V>`
            // bridges.
            crate::query::sql::push_filter_value(acc, v.clone());
        }
        ExprNode::Add(lhs, rhs) => {
            emit_arith(acc, lhs, " + ", rhs);
        }
        ExprNode::Sub(lhs, rhs) => {
            emit_arith(acc, lhs, " - ", rhs);
        }
        ExprNode::Mul(lhs, rhs) => {
            emit_arith(acc, lhs, " * ", rhs);
        }
        ExprNode::Div(lhs, rhs) => {
            emit_arith(acc, lhs, " / ", rhs);
        }
        ExprNode::Cmp { op, lhs, rhs } => {
            emit_expr(acc, lhs);
            acc.push_sql(match op {
                CmpOp::Eq => " = ",
                CmpOp::Neq => " <> ",
                CmpOp::Gt => " > ",
                CmpOp::Gte => " >= ",
                CmpOp::Lt => " < ",
                CmpOp::Lte => " <= ",
            });
            emit_expr(acc, rhs);
        }
        ExprNode::Aggregate {
            op,
            arg,
            filter,
            cast_to: _,
            distinct,
            window: _,
            order_by,
        } => {
            // Bare aggregate emission — keyword, optional DISTINCT, argument,
            // closing paren, optional FILTER clause. The `cast_to` field is
            // intentionally ignored here; the narrowing cast lives at
            // the terminal layer (see
            // [`crate::query::sql::emit_aggregate_with_cast`] and
            // [`crate::query::sql::emit_aggregate_with_window_and_cast`])
            // because its placement depends on whether the aggregate
            // is used as a SELECT scalar (`(AGG(..))::TY`) or inside
            // the annotate SELECT list with a window function
            // (`(AGG(..) OVER ())::TY`). Keeping this arm bare means
            // nested aggregates don't accidentally pick up a cast
            // they never asked for.
            //
            // `CountStar` is the only branch that emits a bare `*`
            // inside the parens and deliberately skips the recursive
            // `emit_expr(arg)` call — the `arg` slot on the typed
            // wrapper carries an inert placeholder for that variant,
            // never a real column reference.
            //
            // The `distinct` flag is checked by
            // `check_aggregate_legality` before emission — rejected
            // combinations (CountStar + DISTINCT, StringAgg + DISTINCT)
            // never reach this arm at fetch time; they fail earlier.
            // At the bare-emit level (unit tests, nested contexts) the
            // flag is still emitted verbatim so tests can verify the
            // flag round-trips correctly.
            // CountStar is special — emits `COUNT(*)` with no DISTINCT
            // hook (the legality check rejects DISTINCT + CountStar
            // upstream of the emitter). StringAgg is special — takes a
            // bound separator after the column expression.
            // Every other variant emits `<KEYWORD>([DISTINCT] <expr>)`
            // through `emit_unary_agg`.
            match op {
                AggOp::CountStar => acc.push_sql("COUNT(*)"),
                AggOp::StringAgg(sep) => {
                    // STRING_AGG with DISTINCT requires a per-aggregate
                    // ORDER BY, enforced by `check_aggregate_legality` at
                    // fetch time. With T1, callers chain
                    // `.order_by(...)` and the `order_by` slot below
                    // emits the well-formed Postgres syntax.
                    // `sep.clone()` is required because `sep` is
                    // `&String` here and `push_bind` takes owned values.
                    acc.push_sql("STRING_AGG(");
                    if *distinct {
                        acc.push_sql("DISTINCT ");
                    }
                    emit_expr(acc, arg);
                    acc.push_sql(", ");
                    acc.push_bind(sep.clone());
                    push_aggregate_order_by(acc, order_by);
                    acc.push_sql(")");
                }
                AggOp::Count => emit_unary_agg(acc, "COUNT(", *distinct, arg, order_by),
                AggOp::Sum => emit_unary_agg(acc, "SUM(", *distinct, arg, order_by),
                AggOp::Avg => emit_unary_agg(acc, "AVG(", *distinct, arg, order_by),
                AggOp::Min => emit_unary_agg(acc, "MIN(", *distinct, arg, order_by),
                AggOp::Max => emit_unary_agg(acc, "MAX(", *distinct, arg, order_by),
                AggOp::ArrayAgg => emit_unary_agg(acc, "ARRAY_AGG(", *distinct, arg, order_by),
                // JSONB_AGG (not JSON_AGG) — Djogi standardises on JSONB
                // for all JSON wire and storage. See docs/spec/decisions.md.
                AggOp::JsonAgg => emit_unary_agg(acc, "JSONB_AGG(", *distinct, arg, order_by),
                // BOOL_AND / BOOL_OR accept DISTINCT (no-op for booleans
                // but valid Postgres syntax).
                AggOp::BoolAnd => emit_unary_agg(acc, "BOOL_AND(", *distinct, arg, order_by),
                AggOp::BoolOr => emit_unary_agg(acc, "BOOL_OR(", *distinct, arg, order_by),
                // EVERY is the SQL-standard alias for BOOL_AND. Both produce
                // identical results in Postgres; the IR carries the alias
                // separately so the emitter renders the keyword the user
                // wrote (call to `.every()` → `EVERY(col)`, never silently
                // rewritten to `BOOL_AND(col)`).
                AggOp::Every => emit_unary_agg(acc, "EVERY(", *distinct, arg, order_by),
                // Bitwise integer aggregates — Postgres returns the
                // operand's own integer type, so no narrowing cast.
                AggOp::BitAnd => emit_unary_agg(acc, "BIT_AND(", *distinct, arg, order_by),
                AggOp::BitOr => emit_unary_agg(acc, "BIT_OR(", *distinct, arg, order_by),
                AggOp::BitXor => emit_unary_agg(acc, "BIT_XOR(", *distinct, arg, order_by),
            }
            // Postgres `AGG(...) FILTER (WHERE <cond>)` runs the
            // filter inside the aggregate's per-row scan — the
            // aggregate ignores rows where `cond` is false. `filter`
            // is None on the bare call site; Some(cond) when
            // `AggregateExpr::filter(...)` was chained.
            //
            // Note: STRING_AGG with FILTER is valid Postgres syntax and
            // works here because the FILTER clause attaches to the whole
            // aggregate function expression after the closing paren.
            if let Some(cond) = filter {
                acc.push_sql(" FILTER (WHERE ");
                emit_expr(acc, cond);
                acc.push_sql(")");
            }
        }
        ExprNode::Case { arms, otherwise } => {
            // CASE WHEN <cond> THEN <val> ... ELSE <default> END.
            //
            // `otherwise` is required by construction (the typed builder
            // surface [`super::case::CaseBuilder::otherwise`] produces
            // the `Expr<V>` only after the caller supplies a default)
            // — no `Option` / `if let` branch here, the ELSE arm is
            // always rendered. This matches the plan's Step 3 decision
            // that a CASE with no matching arm must never silently
            // produce NULL against a column the user expected to be
            // non-null.
            acc.push_sql("CASE ");
            for (cond, val) in arms {
                acc.push_sql("WHEN ");
                emit_expr(acc, cond);
                acc.push_sql(" THEN ");
                emit_expr(acc, val);
                acc.push_sql(" ");
            }
            acc.push_sql("ELSE ");
            emit_expr(acc, otherwise);
            acc.push_sql(" END");
        }
        ExprNode::Exists(sub) => {
            // EXISTS (<subquery>) — subquery renders SELECT 1 because
            // EXISTS only cares about row presence. `emit_subquery`
            // special-cases the `None` select_column arm for exactly
            // this reason; the [`super::subquery::Exists`] typed
            // constructor is the sole producer of this shape and always
            // sets `select_column = None`.
            acc.push_sql("EXISTS (");
            emit_subquery(acc, sub);
            acc.push_sql(")");
        }
        ExprNode::Subquery(sub) => {
            // Scalar subquery — must be wrapped in parens so it slots
            // into arithmetic / comparison positions without re-parsing
            // the outer expression. `emit_subquery` handles the
            // `SELECT <col> FROM ... WHERE ...` body; the outer parens
            // here are structural.
            acc.push_sql("(");
            emit_subquery(acc, sub);
            acc.push_sql(")");
        }
        ExprNode::ArrayLength { column } => {
            // array_length(column, 1) — dimension is always 1 (Djogi arrays
            // are 1-dimensional; multi-dimensional arrays are not supported).
            acc.push_sql("array_length(");
            acc.push_sql(column);
            acc.push_sql(", 1)");
        }
        ExprNode::CurrentYear => {
            // EXTRACT(YEAR FROM CURRENT_DATE) returns Postgres `numeric`; the
            // typed `Expr<i32>` wrapper at the public surface promises an
            // `i32` decode, so the explicit `::INTEGER` cast narrows the
            // result here. No bind parameter — the year is read from the
            // server clock, never user-supplied.
            acc.push_sql("EXTRACT(YEAR FROM CURRENT_DATE)::INTEGER");
        }
        ExprNode::OuterRef { column } => {
            // Outer-scope column reference — emitted unqualified.
            // Postgres resolves the name against the enclosing query
            // scope when the inner `FROM` list has no matching column.
            // Same-named collisions between inner and outer scope
            // trigger `42702 column reference "X" is ambiguous`; the
            // typed surface flags this limitation on
            // [`super::subquery::OuterRef`]. The qualified form is
            // available via [`ExprNode::OuterRefColumn`] for callers
            // that need to disambiguate.
            //
            // Column strings reach this arm only via the sealed macro
            // entry point
            // [`super::subquery::__macro_support::__make_outer_ref`]
            // (macro-emitted code) or the crate-private
            // [`super::subquery::OuterRef::new`] (test / internal
            // helpers) — both run through
            // [`crate::ident::assert_plain_ident`] before the value
            // lands here. Safe to push as a raw SQL token.
            acc.push_sql(column);
        }
        ExprNode::OuterRefColumn { table, column } => {
            // Table-qualified outer-scope column reference — emits
            // `<table>.<column>` to disambiguate same-named columns
            // across the inner and outer scopes (every Djogi model
            // carries `id` / `created_at` / `updated_at`, so framework
            // columns collide on every M2M correlation).
            //
            // Both strings come from [`crate::ident::assert_plain_ident`]-
            // validated identifiers (table from `Model::table_name()`
            // declarations, column from sealed macro entry points).
            // Safe to push as raw SQL tokens.
            acc.push_sql(table);
            acc.push_sql(".");
            acc.push_sql(column);
        }
        ExprNode::IntervalLiteral { microseconds } => {
            // INTERVAL '{N} microseconds' — the full precision of
            // `time::Duration` as a Postgres interval literal. The
            // microsecond count was saturating-clamped to i64 at ExprNode
            // construction time via `expr::literal::saturating_micros`
            // (Durations outside ±292,277 years encode as i64::MAX/MIN).
            // The formatted string consists solely of a decimal integer and
            // the ASCII keyword "microseconds" — no user-controlled text
            // reaches this arm, so pushing raw SQL is safe.
            let literal = format!("INTERVAL '{microseconds} microseconds'");
            acc.push_sql(&literal);
        }

        ExprNode::TsMatch {
            column,
            dictionary,
            query_text,
        } => {
            // `<col> @@ to_tsquery('<dictionary>', $n)`. Column and dictionary
            // identifiers are validated at construction; query_text is user-
            // supplied so it always rides as a bound parameter.
            emit_ts(acc, "", column, dictionary, query_text, " @@ ", ")");
        }
        ExprNode::TsRank {
            column,
            dictionary,
            query_text,
        } => {
            // `ts_rank(<col>, to_tsquery('<dictionary>', $n))`. Standard
            // relevance score — higher = more relevant.
            emit_ts(acc, "ts_rank(", column, dictionary, query_text, ", ", "))");
        }
        ExprNode::TsRankCd {
            column,
            dictionary,
            query_text,
        } => {
            // `ts_rank_cd(...)`. Cover-density variant; weighs term proximity
            // more heavily than `ts_rank`.
            emit_ts(
                acc,
                "ts_rank_cd(",
                column,
                dictionary,
                query_text,
                ", ",
                "))",
            );
        }

        // ── Spatial (gated on `spatial` feature) ───────────────────────────
        #[cfg(feature = "spatial")]
        ExprNode::Spatial(s) => {
            // Delegate entirely to `SpatialExpr::emit`, which handles all
            // bind-parameter placement for PostGIS functions.
            s.emit(acc);
        }
    }
}

/// Emit a Postgres FTS expression — `<prefix><col><sep>to_tsquery('<dictionary>', $n)<suffix>`.
///
/// Three [`ExprNode`] variants share this shape:
///
/// - `TsMatch` — `<col> @@ to_tsquery('<dict>', $n)` (`prefix=""`,
///   `sep=" @@ "`, `suffix=")"`).
/// - `TsRank` — `ts_rank(<col>, to_tsquery('<dict>', $n))` (`prefix="ts_rank("`,
///   `sep=", "`, `suffix="))"`).
/// - `TsRankCd` — same shape with `ts_rank_cd(`.
///
/// `column` is a `&'static str` macro-validated via `assert_plain_ident`.
/// `dictionary` is byte-level validated at attribute parse time; embedded
/// literally as a single-quoted string. `query_text` is user-supplied
/// and always rides through `push_bind`.
fn emit_ts(
    acc: &mut SqlAccumulator,
    prefix: &'static str,
    column: &str,
    dictionary: &str,
    query_text: &str,
    sep: &'static str,
    suffix: &'static str,
) {
    acc.push_sql(prefix);
    acc.push_sql(column);
    acc.push_sql(sep);
    acc.push_sql("to_tsquery('");
    acc.push_sql(dictionary);
    acc.push_sql("', ");
    acc.push_bind(query_text.to_owned());
    acc.push_sql(suffix);
}

/// Emit `<KEYWORD_OPENER>[DISTINCT ]<expr>)`.
///
/// `keyword_opener` is the SQL function name plus opening paren — e.g.
/// `"SUM("`, `"ARRAY_AGG("`. Centralises the eight unary aggregate emit
/// arms (`COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, `ARRAY_AGG`, `JSONB_AGG`,
/// `BOOL_AND`, `BOOL_OR`) so the `[DISTINCT]` placement and parens stay
/// uniform. `STRING_AGG` and `COUNT(*)` are special-cased upstream.
fn emit_unary_agg(
    acc: &mut SqlAccumulator,
    keyword_opener: &'static str,
    distinct: bool,
    arg: &ExprNode,
    order_by: &[crate::query::order::OrderExpr],
) {
    acc.push_sql(keyword_opener);
    if distinct {
        acc.push_sql("DISTINCT ");
    }
    emit_expr(acc, arg);
    push_aggregate_order_by(acc, order_by);
    acc.push_sql(")");
}

/// Emit a per-aggregate `ORDER BY <ord1>, <ord2>, ...` tail when
/// `order_by` is non-empty. Renders inside the aggregate's parens —
/// the caller pushes the closing paren after this returns.
///
/// Aggregates do not participate in the join / parent-table-qualifier
/// flow that `QuerySet`-level ordering uses, so the qualifier slot is
/// always `None` here. Each `OrderExpr::emit` call writes its column
/// reference without table qualification.
fn push_aggregate_order_by(acc: &mut SqlAccumulator, order_by: &[crate::query::order::OrderExpr]) {
    if order_by.is_empty() {
        return;
    }
    acc.push_sql(" ORDER BY ");
    for (i, o) in order_by.iter().enumerate() {
        if i > 0 {
            acc.push_sql(", ");
        }
        o.emit(acc, None);
    }
}

/// Emit a [`SubqueryNode`] body — `SELECT <col or 1> FROM <table>
/// [WHERE <condition>]`.
///
/// Shared by both [`ExprNode::Exists`] and [`ExprNode::Subquery`] — they
/// differ only in (a) whether the node carries a `select_column` and
/// (b) whether the outer arm wraps the result in `EXISTS (..)` / `(..)`.
/// `emit_subquery` itself renders the common body; the outer wrap lives
/// at the arm level.
///
/// # Why `emit_condition` and not a second [`emit_expr`] walk?
///
/// The subquery's `WHERE` clause is a [`Condition`] tree (not an
/// [`ExprNode`]) because it was built through
/// [`crate::query::QuerySet::filter`] / [`crate::query::QuerySet::filter_expr`]
/// — those accumulate `Condition` with a full `LookupOp` vocabulary
/// (ILIKE, BETWEEN, IS NULL, IN list, …). Reusing
/// [`crate::query::sql::emit_condition`] lets every lookup op compose
/// inside a subquery without a parallel `ExprNode`-side emitter, which
/// would duplicate every `LookupOp` arm and drift over time. The tradeoff
/// is that [`super::subquery::Exists::new`] / [`super::subquery::Subquery::new`]
/// clone the inner queryset's condition tree at construction time; that
/// clone is cheap (the tree is shallow Vec<_> + enum variants) and the
/// correlated-subquery build sites are a hot-path outlier, not the norm.
fn emit_subquery(acc: &mut SqlAccumulator, node: &SubqueryNode) {
    acc.push_sql("SELECT ");
    match &node.select_column {
        Some(col) => {
            // `col` is a `&'static str` from
            // [`crate::query::field::FieldRef::column`], validated at
            // `FieldRef::new` construction time. Safe to push raw.
            acc.push_sql(col);
        }
        None => {
            // EXISTS path — the constant 1 stands in for "some value"
            // because EXISTS ignores column values entirely. Using the
            // literal `1` rather than `*` avoids an unnecessary SELECT-
            // list expansion on the planner side and matches the
            // idiomatic Postgres form for EXISTS subqueries.
            acc.push_sql("1");
        }
    }
    acc.push_sql(" FROM ");
    // Table name is always `<T as Model>::table_name()` from the typed
    // surface — macro-baked, never user input.
    acc.push_sql(node.table);
    if let Some(cond) = &node.where_clause {
        acc.push_sql(" WHERE ");
        // Clone: `emit_condition` consumes its `Condition` input by
        // value (payload strings / boxed values move into bind calls).
        // The subquery tree is referenced, not owned, because a single
        // `SubqueryNode` may be emitted more than once if, e.g., a
        // retry path re-emits the same outer `ExprNode`. The clone is
        // structural (Vec / Box / enum variants; no deep data), so the
        // cost is proportional to the filter's shape, not the row
        // count the filter evaluates against.
        //
        // `parent_table = None` — the subquery's own table is the
        // primary `FROM` source, so bare column references in the
        // subquery's WHERE resolve to it unambiguously; qualified
        // emission waits for the broader `parent_table` threading
        // change flagged in `expr::sql`'s header comment.
        crate::query::sql::emit_condition(acc, cond.clone(), None);
    }
}

/// Emit an arithmetic binary node with parens around any arithmetic
/// sub-expression.
///
/// SQL precedence binds `*` / `/` tighter than `+` / `-`, so a
/// Rust-built tree like `Mul(Add(a, b), c)` would silently re-parse as
/// `a + (b * c)` if we emitted `a + b * c`. Wrapping every arithmetic
/// sub-expression in explicit parens preserves the structural grouping
/// the user wrote. The outer arm still picks up its own operator
/// between the two wrapped sides.
///
/// Non-arithmetic operands (field refs, literals, comparisons,
/// aggregates) don't need wrapping — they're already single tokens or
/// already self-parenthesised — so the wrap is gated on the sub-node's
/// discriminant.
fn emit_arith(acc: &mut SqlAccumulator, lhs: &ExprNode, op: &'static str, rhs: &ExprNode) {
    emit_wrapped_if_arith(acc, lhs);
    acc.push_sql(op);
    emit_wrapped_if_arith(acc, rhs);
}

fn emit_wrapped_if_arith(acc: &mut SqlAccumulator, node: &ExprNode) {
    match node {
        ExprNode::Add(..) | ExprNode::Sub(..) | ExprNode::Mul(..) | ExprNode::Div(..) => {
            acc.push_sql("(");
            emit_expr(acc, node);
            acc.push_sql(")");
        }
        _ => emit_expr(acc, node),
    }
}

#[cfg(test)]
mod tests {
    //! Emitter unit tests — assert the generated SQL text for each
    //! `ExprNode` variant combination the public API can produce.
    //! `SqlAccumulator::sql()` exposes the text with bind placeholders as
    //! `$1`, `$2`, … so we can count the bind slots without actually
    //! running the query.
    //!
    //! The tests reach `FieldRef::new` via its `pub(crate)` constructor
    //! — expr lives in the same crate, so direct construction is fine.
    //! Column strings (`"view_count"`, `"author_id"`, etc.) satisfy
    //! `assert_plain_ident` so the seal on `__make_field_ref` is not
    //! exercised here; it has its own unit coverage in
    //! `query::field::__macro_support::tests`.
    use super::*;
    use crate::Expr;
    use crate::descriptor::ModelDescriptor;
    use crate::pg::accumulator::SqlAccumulator;
    use crate::query::field::FieldRef;

    // Inert local model — only `table_name` and the trait bounds
    // matter for unit tests that never run SQL. Mirrors the `Fake`
    // stub in `query::field::tests` and `query::sql::tests`.
    struct Post;
    impl crate::model::__sealed::Sealed for Post {}
    #[allow(clippy::manual_async_fn)]
    impl crate::model::Model for Post {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "posts"
        }
        fn pk_value(&self) -> &i64 {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            unreachable!()
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: i64,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
    }

    #[test]
    fn emit_field_vs_literal_eq() {
        // `view_count.as_expr().eq(Expr::literal(100))` must emit
        // `view_count = $1` — one bind slot for the literal, bare
        // column reference for the field side.
        let f: FieldRef<Post, i32> = FieldRef::new("view_count");
        let expr = f.as_expr().eq(Expr::literal(100i32));
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &expr.node);
        let sql = acc.sql();
        assert!(sql.contains("view_count = $1"), "got: {sql}");
    }

    #[test]
    fn emit_field_vs_field_eq() {
        // `author_id.as_expr().eq(editor_id.as_expr())` must emit
        // `author_id = editor_id` with zero bind slots — both sides
        // are column references.
        let a: FieldRef<Post, i64> = FieldRef::new("author_id");
        let b: FieldRef<Post, i64> = FieldRef::new("editor_id");
        let expr = a.as_expr().eq(b.as_expr());
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &expr.node);
        let sql = acc.sql();
        assert_eq!(sql.trim(), "author_id = editor_id");
        assert!(!sql.contains('$'), "no binds expected, got: {sql}");
    }

    #[test]
    fn emit_arithmetic_add_literal() {
        // `view_count.as_expr() + Expr::literal(1)` must emit
        // `view_count + $1` — one bind slot for the literal RHS,
        // bare `+` operator token between the two operands.
        let f: FieldRef<Post, i32> = FieldRef::new("view_count");
        let expr = f.as_expr() + Expr::literal(1i32);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &expr.node);
        let sql = acc.sql();
        assert!(sql.contains("view_count + $1"), "got: {sql}");
    }

    #[test]
    fn emit_arithmetic_mixed_precedence_wraps_inner_ops() {
        // Rust-level `(a + b) * c` builds `Mul(Add(a, b), c)`. Without
        // grouping, the emitter would produce `a + b * c` which SQL
        // binds as `a + (b * c)` — the opposite of what the user wrote.
        // Every arithmetic sub-expression must be wrapped in explicit
        // parens to preserve the structural grouping.
        let a: FieldRef<Post, i32> = FieldRef::new("a");
        let b: FieldRef<Post, i32> = FieldRef::new("b");
        let c: FieldRef<Post, i32> = FieldRef::new("c");
        let expr = (a.as_expr() + b.as_expr()) * c.as_expr();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &expr.node);
        let sql = acc.sql();
        assert_eq!(sql.trim(), "(a + b) * c", "got: {sql}");
    }

    #[test]
    fn emit_arithmetic_mixed_precedence_wraps_rhs_ops() {
        // `a + (b - c)` builds `Add(a, Sub(b, c))`. Rust already picked
        // the grouping; the emitter must echo it. Without the rhs
        // wrap the SQL would be `a + b - c` which re-parses left-
        // associative as `(a + b) - c` — wrong.
        let a: FieldRef<Post, i32> = FieldRef::new("a");
        let b: FieldRef<Post, i32> = FieldRef::new("b");
        let c: FieldRef<Post, i32> = FieldRef::new("c");
        let expr = a.as_expr() + (b.as_expr() - c.as_expr());
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &expr.node);
        let sql = acc.sql();
        assert_eq!(sql.trim(), "a + (b - c)", "got: {sql}");
    }

    #[test]
    fn emit_arithmetic_flat_left_associative_no_spurious_parens() {
        // `a + b + c` builds `Add(Add(a, b), c)` via left-to-right
        // evaluation of the `+` operator. The inner `Add(a, b)` IS an
        // arithmetic sub-expression, so the emitter wraps it. That's
        // structurally accurate — `(a + b) + c` and `a + b + c` are
        // semantically identical, and the explicit grouping hurts
        // nothing. The test pins the shipped behaviour so a future
        // optimisation that elides parens for associative peers
        // (same-op chains) has an explicit decision point.
        let a: FieldRef<Post, i32> = FieldRef::new("a");
        let b: FieldRef<Post, i32> = FieldRef::new("b");
        let c: FieldRef<Post, i32> = FieldRef::new("c");
        let expr = a.as_expr() + b.as_expr() + c.as_expr();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &expr.node);
        let sql = acc.sql();
        assert_eq!(sql.trim(), "(a + b) + c", "got: {sql}");
    }

    // ── T20: Expr::current_year() — Cluster C C2 ──────────────────────────────

    /// `Expr::current_year()` emits the bare
    /// `EXTRACT(YEAR FROM CURRENT_DATE)::INTEGER` token stream with no
    /// bind parameters — the year is read from the server clock, never
    /// supplied by the caller.
    #[test]
    fn emit_current_year_no_binds() {
        let expr = Expr::current_year();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &expr.node);
        let sql = acc.sql();
        assert_eq!(
            sql.trim(),
            "EXTRACT(YEAR FROM CURRENT_DATE)::INTEGER",
            "got: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            0,
            "current_year takes no user input — must bind zero params; got {}",
            acc.bind_count()
        );
    }

    /// `Expr::current_year() - field.as_expr()` — the canonical age
    /// expression — must lower to
    /// `EXTRACT(YEAR FROM CURRENT_DATE)::INTEGER - <col>` via the existing
    /// `Expr<i32>` arithmetic IR. The arithmetic emitter wraps each side
    /// in parens because `CurrentYear` is a non-trivial expression token
    /// (it contains its own `::` cast operator), so the canonical SQL
    /// shape includes the structural parens that pin the operator
    /// precedence.
    #[test]
    fn emit_current_year_minus_field_age_expression() {
        let f: FieldRef<Post, i32> = FieldRef::new("estimated_birth_year");
        let age = Expr::current_year() - f.as_expr();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &age.node);
        let sql = acc.sql();
        // The token stream must reference both halves and the subtraction
        // operator. Existing arithmetic emitter wraps each side in parens
        // when it is itself an expression token; the bare column on the
        // RHS does not get wrapped, but the LHS's `EXTRACT(...)::INTEGER`
        // form contains an arithmetic-adjacent `::` cast so the emitter's
        // structural-parens contract makes the layout deterministic.
        assert!(
            sql.contains("EXTRACT(YEAR FROM CURRENT_DATE)::INTEGER"),
            "expected current_year token, got: {sql}"
        );
        assert!(
            sql.contains(" - estimated_birth_year"),
            "expected subtraction with column on RHS, got: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            0,
            "neither side binds a parameter; got {}",
            acc.bind_count()
        );
    }

    /// The age expression composes with comparisons — `(year - col).gte(15)`
    /// must yield a bound RHS literal alongside the year/column tokens.
    #[test]
    fn emit_current_year_age_with_gte_threshold() {
        let f: FieldRef<Post, i32> = FieldRef::new("estimated_birth_year");
        let predicate = (Expr::current_year() - f.as_expr()).gte(Expr::literal(15i32));
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &predicate.node);
        let sql = acc.sql();
        assert!(
            sql.contains("EXTRACT(YEAR FROM CURRENT_DATE)::INTEGER"),
            "got: {sql}"
        );
        assert!(sql.contains("estimated_birth_year"), "got: {sql}");
        assert!(sql.contains(" >= $1"), "got: {sql}");
        assert_eq!(
            acc.bind_count(),
            1,
            "exactly one bind for the threshold; got {}",
            acc.bind_count()
        );
    }
}
