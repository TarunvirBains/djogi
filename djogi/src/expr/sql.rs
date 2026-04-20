//! Expression-IR → SQL emitter.
//!
//! # What
//!
//! [`emit_expr`] walks an [`ExprNode`] tree and pushes the matching SQL
//! tokens + bind parameters onto a [`sqlx::QueryBuilder<'_, Postgres>`].
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
//! - [`ExprNode::Field { column }`] — `qb.push(*column)`. The column
//!   name is a `&'static str` validated at
//!   [`crate::query::field::FieldRef::new`] construction time against
//!   [`crate::ident::assert_plain_ident`]; no re-validation here.
//! - [`ExprNode::Literal(v)`] — delegates to
//!   [`crate::query::sql::push_filter_value`], which calls
//!   `qb.push_bind(v)` for every scalar variant. All user-supplied
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

use crate::expr::node::{CmpOp, ExprNode};
use sqlx::{Postgres, QueryBuilder};

/// Walk an [`ExprNode`] and push the corresponding SQL fragment onto
/// `qb`. Leaves consume bind slots (via
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
///   [`crate::query::field::FieldRef::new`]); safe to `qb.push(*column)`.
pub(crate) fn emit_expr(qb: &mut QueryBuilder<'_, Postgres>, node: &ExprNode) {
    match node {
        ExprNode::Field { column } => {
            // Bare column reference — validated at FieldRef construction.
            qb.push(*column);
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
            crate::query::sql::push_filter_value(qb, v.clone());
        }
        ExprNode::Add(lhs, rhs) => {
            emit_expr(qb, lhs);
            qb.push(" + ");
            emit_expr(qb, rhs);
        }
        ExprNode::Sub(lhs, rhs) => {
            emit_expr(qb, lhs);
            qb.push(" - ");
            emit_expr(qb, rhs);
        }
        ExprNode::Mul(lhs, rhs) => {
            emit_expr(qb, lhs);
            qb.push(" * ");
            emit_expr(qb, rhs);
        }
        ExprNode::Div(lhs, rhs) => {
            emit_expr(qb, lhs);
            qb.push(" / ");
            emit_expr(qb, rhs);
        }
        ExprNode::Cmp { op, lhs, rhs } => {
            emit_expr(qb, lhs);
            qb.push(match op {
                CmpOp::Eq => " = ",
                CmpOp::Neq => " <> ",
                CmpOp::Gt => " > ",
                CmpOp::Gte => " >= ",
                CmpOp::Lt => " < ",
                CmpOp::Lte => " <= ",
            });
            emit_expr(qb, rhs);
        }
    }
}

#[cfg(test)]
mod tests {
    //! Emitter unit tests — assert the generated SQL text for each
    //! `ExprNode` variant combination the public API can produce.
    //! `QueryBuilder::sql()` exposes the text with bind placeholders as
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
    use crate::query::field::FieldRef;
    use sqlx::QueryBuilder;

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
        let mut qb: QueryBuilder<'_, Postgres> = QueryBuilder::new("");
        emit_expr(&mut qb, &expr.node);
        let sql = qb.sql();
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
        let mut qb: QueryBuilder<'_, Postgres> = QueryBuilder::new("");
        emit_expr(&mut qb, &expr.node);
        let sql = qb.sql();
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
        let mut qb: QueryBuilder<'_, Postgres> = QueryBuilder::new("");
        emit_expr(&mut qb, &expr.node);
        let sql = qb.sql();
        assert!(sql.contains("view_count + $1"), "got: {sql}");
    }
}
