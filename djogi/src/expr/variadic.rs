//! Variadic-function constructors on [`Expr<T>`] — `COALESCE` / `LEAST` /
//! `GREATEST`. Generic over `T` (no numeric bound: Postgres orders
//! server-side, matching the comparison surface in [`super::compare`]).
//! Each two-operand `least`/`greatest` form takes `impl Into<Expr<T>>` so
//! literals, field refs, and composed expressions all interop; the `_many`
//! forms take a `Vec<Expr<T>>` (an empty list surfaces an emit-time error,
//! since Postgres rejects the zero-argument forms).
//!
//! [`Expr::coalesce`] is the one asymmetric constructor: its first operand is
//! `Expr<Option<T>>` (the nullable column being defaulted) and its second is
//! `Expr<T>` (the non-null fallback), matching the actual return type of
//! `f.col().as_expr()` for an `Option<T>` column.

use crate::expr::Expr;
use crate::expr::node::ExprNode;

impl<T> Expr<T> {
    /// `COALESCE(nullable, fallback)` — first non-NULL operand; the
    /// nullable-default common case (`COALESCE(nullable_col, default)`).
    /// `nullable` is `Expr<Option<T>>` (a nullable column reference —
    /// `f.score().as_expr()` for an `Option<T>` column is already
    /// `Expr<Option<T>>`); `fallback` is `Expr<T>` (a non-null default).
    /// Emits `COALESCE($1, $2)`. For N homogeneous operands use
    /// [`Self::coalesce_many`].
    /// # Example
    /// ```ignore
    /// // COALESCE(score, 0) — default a nullable score column to 0.
    /// let scored = Expr::coalesce(f.score().as_expr(), Expr::literal(0i32));
    /// ```
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn coalesce(nullable: impl Into<Expr<Option<T>>>, fallback: impl Into<Expr<T>>) -> Expr<T> {
        let nullable: Expr<Option<T>> = nullable.into();
        let fallback: Expr<T> = fallback.into();
        Expr::from_node(ExprNode::Coalesce(vec![nullable.node, fallback.node]))
    }

    /// `COALESCE(e1, e2, …, eN)` — first non-NULL of N homogeneous operands.
    /// The `Vec` must be non-empty; an empty list surfaces a
    /// [`crate::query::PortablePredicateError`] at SQL-emit time (Postgres
    /// rejects `COALESCE()` with no args). Prefer [`Self::coalesce`] for the
    /// two-operand nullable-default case.
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn coalesce_many(exprs: Vec<Expr<T>>) -> Expr<T> {
        Expr::from_node(ExprNode::Coalesce(
            exprs.into_iter().map(|e| e.node).collect(),
        ))
    }

    /// `LEAST(a, b)` — smaller non-NULL operand (NULL only if both are NULL).
    /// N-operand form: [`Self::least_many`].
    /// # Example
    /// ```ignore
    /// // LEAST(cap, requested) — clamp a requested amount to a cap.
    /// let clamped = Expr::least(f.cap().as_expr(), Expr::literal(100i32));
    /// ```
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn least(a: impl Into<Expr<T>>, b: impl Into<Expr<T>>) -> Expr<T> {
        Expr::from_node(ExprNode::Least(vec![a.into().node, b.into().node]))
    }

    /// `LEAST(e1, …, eN)` — smallest non-NULL operand. Non-empty `Vec`;
    /// empty surfaces an emit-time [`crate::query::PortablePredicateError`].
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn least_many(exprs: Vec<Expr<T>>) -> Expr<T> {
        Expr::from_node(ExprNode::Least(exprs.into_iter().map(|e| e.node).collect()))
    }

    /// `GREATEST(a, b)` — larger non-NULL operand (NULL only if both are NULL).
    /// N-operand form: [`Self::greatest_many`].
    /// # Example
    /// ```ignore
    /// // GREATEST(floor, current) — floor a current value.
    /// let floored = Expr::greatest(f.floor().as_expr(), Expr::literal(0i32));
    /// ```
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn greatest(a: impl Into<Expr<T>>, b: impl Into<Expr<T>>) -> Expr<T> {
        Expr::from_node(ExprNode::Greatest(vec![a.into().node, b.into().node]))
    }

    /// `GREATEST(e1, …, eN)` — largest non-NULL operand. Non-empty `Vec`;
    /// empty surfaces an emit-time [`crate::query::PortablePredicateError`].
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn greatest_many(exprs: Vec<Expr<T>>) -> Expr<T> {
        Expr::from_node(ExprNode::Greatest(
            exprs.into_iter().map(|e| e.node).collect(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use crate::expr::Expr;
    use crate::expr::node::ExprNode;
    use crate::expr::sql::emit_expr;
    use crate::pg::accumulator::SqlAccumulator;
    use crate::query::portable::SqlEmitContext;

    #[test]
    fn coalesce_two_operands_emits_coalesce() {
        // Nullable Expr<Option<f32>> as first arg, Expr<f32> fallback.
        let nullable: Expr<Option<f32>> = Expr::from_node(ExprNode::Field { column: "score" });
        let fallback: Expr<f32> = Expr::literal(0.0f32);
        let result = Expr::<f32>::coalesce(nullable, fallback);
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &result.node, SqlEmitContext::root()).expect("coalesce emission");
        assert_eq!(qb.sql().trim(), "COALESCE(score, $1)", "got: {}", qb.sql());
    }

    #[test]
    fn coalesce_many_rejects_empty_at_emit() {
        // coalesce_many with an empty Vec must surface a guard, never emit
        // `COALESCE()` (a Postgres syntax error).
        let e: Expr<i32> = Expr::coalesce_many(Vec::new());
        let mut qb = SqlAccumulator::new("");
        let res = emit_expr(&mut qb, &e.node, SqlEmitContext::root());
        assert!(
            res.is_err(),
            "empty COALESCE must error at emit, got: {}",
            qb.sql()
        );
    }

    #[test]
    fn least_two_operands_emits_least() {
        let e: Expr<i64> = Expr::least(Expr::literal(5i64), Expr::literal(9i64));
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &e.node, SqlEmitContext::root()).expect("least emission");
        assert_eq!(qb.sql().trim(), "LEAST($1, $2)", "got: {}", qb.sql());
    }

    #[test]
    fn greatest_two_operands_emits_greatest() {
        let e: Expr<i64> = Expr::greatest(Expr::literal(5i64), Expr::literal(9i64));
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &e.node, SqlEmitContext::root()).expect("greatest emission");
        assert_eq!(qb.sql().trim(), "GREATEST($1, $2)", "got: {}", qb.sql());
    }

    #[test]
    fn least_many_rejects_empty_at_emit() {
        let e: Expr<i32> = Expr::least_many(Vec::new());
        let mut qb = SqlAccumulator::new("");
        let res = emit_expr(&mut qb, &e.node, SqlEmitContext::root());
        assert!(
            res.is_err(),
            "empty LEAST must error at emit, got: {}",
            qb.sql()
        );
    }

    #[test]
    fn greatest_many_rejects_empty_at_emit() {
        let e: Expr<i32> = Expr::greatest_many(Vec::new());
        let mut qb = SqlAccumulator::new("");
        let res = emit_expr(&mut qb, &e.node, SqlEmitContext::root());
        assert!(
            res.is_err(),
            "empty GREATEST must error at emit, got: {}",
            qb.sql()
        );
    }

    #[test]
    fn now_emits_now_call() {
        let e: Expr<time::OffsetDateTime> = Expr::now();
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &e.node, SqlEmitContext::root()).expect("now emission");
        assert_eq!(qb.sql().trim(), "NOW()", "got: {}", qb.sql());
    }
}
