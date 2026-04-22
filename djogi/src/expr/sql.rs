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
//! The Phase 2 [`crate::query::sql::emit_leaf`] handles `column op
//! literal` — the left side is always a bare column name, the right side
//! always a literal. The expression IR generalises both sides: either
//! can be a column, a literal, or a nested arithmetic expression. A
//! recursive emitter is the natural fit; factoring it into its own
//! function keeps the Phase 2 leaf path (with its `parent_table`
//! qualification + `ILIKE` escape rules) un-entangled from the new
//! recursive walk.
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
//! Phase 3 Task 5 added a `parent_table: Option<&'static str>` argument
//! to [`crate::query::sql::emit_condition`] so that `select_related`
//! joined queries qualify bare column references as `{table}.{col}`.
//! The expression IR does **not** carry that argument today. Phase 4
//! Task 3a's scope is single-table expressions (field-vs-field, arithmetic,
//! literal), and joined expressions need a separate design pass that
//! answers ownership questions (which table owns `OuterRef { column }`?
//! which child table sources an aggregate over a joined collection?).
//! When that design lands (Task 5 or later), `emit_expr` grows a
//! `parent_table` parameter and `ExprNode::Field` arms qualify accordingly.
//!
//! For today, `Condition::Expr` inside a `select_related` filter will
//! emit a bare column reference, which Postgres will flag as ambiguous
//! if the child contributes a same-named column. Users can avoid this
//! by staying on the Phase 2 `filter` closure when combining with
//! `select_related`; `filter_expr` is aimed at non-joined predicates
//! until Task 5.

use crate::expr::node::{AggOp, CmpOp, ExprNode, SubqueryNode};
use crate::pg::accumulator::SqlAccumulator;

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
        } => {
            // Bare aggregate emission — keyword, argument, closing
            // paren, optional FILTER clause. The `cast_to` field is
            // intentionally ignored here; the narrowing cast lives at
            // the terminal layer (see
            // [`crate::query::sql::emit_aggregate_with_cast`] and
            // [`crate::query::sql::emit_aggregate_with_window_and_cast`])
            // because its placement depends on whether the aggregate
            // is used as a SELECT scalar (`(AGG(..))::TY`) or inside
            // the annotate SELECT list with a window function
            // (`(AGG(..) OVER ())::TY`). Keeping this arm bare means
            // nested aggregates (Phase 5) don't accidentally pick up a
            // cast they never asked for.
            //
            // `CountStar` is the only branch that emits a bare `*`
            // inside the parens and deliberately skips the recursive
            // `emit_expr(arg)` call — the `arg` slot on the typed
            // wrapper carries an inert placeholder for that variant,
            // never a real column reference.
            match op {
                AggOp::Count => {
                    acc.push_sql("COUNT(");
                    emit_expr(acc, arg);
                    acc.push_sql(")");
                }
                AggOp::CountStar => {
                    acc.push_sql("COUNT(*)");
                }
                AggOp::Sum => {
                    acc.push_sql("SUM(");
                    emit_expr(acc, arg);
                    acc.push_sql(")");
                }
                AggOp::Avg => {
                    acc.push_sql("AVG(");
                    emit_expr(acc, arg);
                    acc.push_sql(")");
                }
                AggOp::Min => {
                    acc.push_sql("MIN(");
                    emit_expr(acc, arg);
                    acc.push_sql(")");
                }
                AggOp::Max => {
                    acc.push_sql("MAX(");
                    emit_expr(acc, arg);
                    acc.push_sql(")");
                }
                AggOp::ArrayAgg => {
                    // ARRAY_AGG(col) — collects non-null values into a
                    // Postgres array. Decoded as Vec<V> at the Rust level.
                    acc.push_sql("ARRAY_AGG(");
                    emit_expr(acc, arg);
                    acc.push_sql(")");
                }
                AggOp::JsonAgg => {
                    // JSONB_AGG(col) — Djogi standardises on JSONB for all
                    // JSON wire and storage formats, so JSON_AGG is never
                    // emitted. See docs/spec/decisions.md.
                    acc.push_sql("JSONB_AGG(");
                    emit_expr(acc, arg);
                    acc.push_sql(")");
                }
                AggOp::StringAgg(sep) => {
                    // STRING_AGG(col, $sep) — separator is bound as a
                    // parameter to guard against injection from a
                    // runtime-computed separator string. The clone is
                    // required because `sep` is `&String` (borrowed from
                    // the ExprNode) and `push_bind` takes owned values.
                    acc.push_sql("STRING_AGG(");
                    emit_expr(acc, arg);
                    acc.push_sql(", ");
                    acc.push_bind(sep.clone());
                    acc.push_sql(")");
                }
                AggOp::BoolAnd => {
                    // BOOL_AND(col) — true if every non-null value is true.
                    acc.push_sql("BOOL_AND(");
                    emit_expr(acc, arg);
                    acc.push_sql(")");
                }
                AggOp::BoolOr => {
                    // BOOL_OR(col) — true if at least one non-null value is
                    // true.
                    acc.push_sql("BOOL_OR(");
                    emit_expr(acc, arg);
                    acc.push_sql(")");
                }
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
        ExprNode::OuterRef { column } => {
            // Outer-scope column reference — emitted unqualified.
            // Postgres resolves the name against the enclosing query
            // scope when the inner `FROM` list has no matching column.
            // Same-named collisions between inner and outer scope
            // trigger `42702 column reference "X" is ambiguous`; the
            // typed surface flags this limitation on
            // [`super::subquery::OuterRef`]. The qualified form is
            // deferred alongside the `parent_table` threading for
            // `select_related + filter_expr` composition.
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
        ExprNode::IntervalLiteral { microseconds } => {
            // INTERVAL '{N} microseconds' — the full precision of
            // `time::Duration` as a Postgres interval literal. The
            // microsecond count was clamped to i64 at ExprNode
            // construction time (see `expr::node` doc). The formatted
            // string consists solely of a decimal integer and the ASCII
            // keyword "microseconds" — no user-controlled text reaches
            // this arm, so pushing raw SQL is safe.
            let literal = format!("INTERVAL '{microseconds} microseconds'");
            acc.push_sql(&literal);
        }
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
/// [`ExprNode`]) because it was built through the Phase 2
/// [`crate::query::QuerySet::filter`] / [`crate::query::QuerySet::filter_expr`]
/// path — those accumulate `Condition` with a full `LookupOp` vocabulary
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
}
