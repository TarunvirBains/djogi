//! `CASE WHEN... THEN... ELSE... END` — multi-armed conditional
//! expression builder.
//! # What
//! [`Case::when`] opens a builder; [`CaseBuilder::when`] appends arms;
//! [`CaseBuilder::otherwise`] closes the builder and produces the typed
//! [`Expr<V>`]. The value type `V` is pinned by the first arm's `then`
//! operand (the turbofish `Case::<V>::when(..)` is never needed at the
//! call site because Rust infers `V` from the argument type).
//! # Why a builder
//! CASE is always at least two arms (WHEN + ELSE). Calling `Case::when`
//! alone does not produce an `Expr<V>` — it produces a
//! [`CaseBuilder<V>`] whose only path to `Expr<V>` is
//! [`CaseBuilder::otherwise`]. This makes "forgot the ELSE arm" a type
//! error at the call site rather than a silent runtime SQL failure or a
//! column that unexpectedly produces NULL when no arm fires. The plan's
//! Step 3 decision — "`otherwise` is REQUIRED — forces the user to
//! decide" — is enforced by this type-state transition, not by a runtime
//! check.
//! # Multi-arm CASE
//! ```ignore
//! use djogi::prelude::*;
//! use djogi::expr::case::Case;
//!
//! let status = Case::when(
//!  f.balance().as_expr().lt(Expr::literal(0i64)),
//!  Expr::literal("overdrawn".to_string()),
//! )
//!.when(
//!  f.balance().as_expr().eq(Expr::literal(0i64)),
//!  Expr::literal("zero".to_string()),
//! )
//!.otherwise(Expr::literal("ok".to_string()));
//! ```
//! Each arm's `cond` is `Expr<bool>` and `then_val` is `Expr<V>` (the
//! same `V` the builder carries). The final `Expr<V>` slots into
//! [`crate::query::field::FieldRef::set_expr`] for CASE-backed UPDATE
//! assignments, or into a nested expression for CASE-backed
//! filter / select-list positions.
//! # Where
//! - [`super::node::ExprNode::Case`] — the untyped payload.
//! - [`super::sql::emit_expr`] — renders the SQL tokens (one arm per
//! `(cond, val)` pair, then `ELSE <default> END`).

use crate::expr::Expr;
use crate::expr::node::ExprNode;
use std::marker::PhantomData;

/// CASE-expression constructor — the entry point for building a CASE
/// tree. `Case::when(cond, then)` returns a [`CaseBuilder<V>`]; chain
/// additional `.when(..)` calls for multi-arm CASE, then close with
/// `.otherwise(default)` to produce the typed [`Expr<V>`].
/// `Case<V>` itself never materialises an instance — the builder pattern
/// lives on [`CaseBuilder<V>`]. `Case<V>` exists purely as a namespace
/// for the [`Case::when`] entry point; the `V` generic on the type
/// itself is inferred from the first `then_val` argument at the call
/// site.
pub struct Case<V> {
    // Uninhabited-by-convention: never constructed. The type exists
    // solely as a namespace for the `when` entry point. `PhantomData`
    // keeps `V` covariant (matching `Expr<V>`'s variance) without
    // requiring an owned `V` field.
    _never: PhantomData<fn() -> V>,
}

impl<V> Case<V> {
    /// Open a CASE builder with the first `(cond, then_val)` arm.
    /// Returns a [`CaseBuilder<V>`] — **not** an [`Expr<V>`]. The
    /// builder's only public path to `Expr<V>` is
    /// [`CaseBuilder::otherwise`], which requires the caller to supply
    /// an `ELSE <default>` arm. That type-state transition is the
    /// "otherwise is required" enforcement — no runtime check needed.
    /// # Type inference
    /// `V` is inferred from `then_val`'s type parameter. The builder
    /// methods pin `V` across every subsequent `.when(..)` call, so a
    /// caller that accidentally mixes value types (e.g. a `String`
    /// then-value followed by an `i64` then-value) gets a compile error
    /// at the mixed arm.
    pub fn when(cond: Expr<bool>, then_val: Expr<V>) -> CaseBuilder<V> {
        CaseBuilder {
            arms: vec![(cond.node, then_val.node)],
            _v: PhantomData,
        }
    }
}

/// In-progress CASE tree. Grown via [`CaseBuilder::when`]; closed into
/// the typed [`Expr<V>`] by [`CaseBuilder::otherwise`].
/// `#[must_use]` because a `CaseBuilder` that never reaches `.otherwise`
/// is almost always a mistake — the caller built arms that will never
/// be emitted into SQL. The drop-without-otherwise case is structurally
/// flagged here; the type-state also prevents `.otherwise`-less use
/// from silently type-checking (the builder is not itself an `Expr<V>`,
/// so any site expecting `Expr<V>` will reject it).
#[must_use = "CaseBuilder must be closed with.otherwise(default) to produce an Expr<V>"]
pub struct CaseBuilder<V> {
    /// Accumulated `(condition, value)` pairs in emission order. The
    /// emitter walks this Vec and renders `WHEN... THEN...` arms in
    /// order; Postgres picks the first arm whose condition evaluates
    /// to true.
    arms: Vec<(ExprNode, ExprNode)>,
    /// Covariant `V` tag — every appended arm and the final
    /// `otherwise` default must be `Expr<V>` for the same `V`. The
    /// `fn() -> V` shape keeps the builder `Send + Sync` regardless of
    /// `V`'s own markers, mirroring [`Expr<V>`]'s variance.
    _v: PhantomData<fn() -> V>,
}

impl<V> CaseBuilder<V> {
    /// Append another `WHEN <cond> THEN <val>` arm.
    /// Arms are ordered — Postgres picks the first arm whose condition
    /// is true. Callers who need "most-specific-first" semantics should
    /// append accordingly; there is no automatic reordering because
    /// the builder would have no way to know what "specific" means at
    /// the expression-IR level.
    pub fn when(mut self, cond: Expr<bool>, then_val: Expr<V>) -> Self {
        self.arms.push((cond.node, then_val.node));
        self
    }

    /// Close the builder with the `ELSE <default>` arm and produce the
    /// typed [`Expr<V>`].
    /// The default is required — a CASE without one produces NULL when
    /// no arm fires, which is rarely the intended semantics. Forcing
    /// the caller to supply a default converts "forgot the fallback"
    /// from a silent runtime bug into a compile-time type-state
    /// mismatch: there is no path from [`CaseBuilder<V>`] to
    /// [`Expr<V>`] that does not pass through this method.
    pub fn otherwise(self, default: Expr<V>) -> Expr<V> {
        Expr::from_node(ExprNode::Case {
            arms: self.arms,
            otherwise: Box::new(default.node),
        })
    }
}

#[cfg(test)]
mod tests {
    //! Emitter shape tests — assert the SQL tokens produced for a
    //! single-arm CASE and a multi-arm CASE. Live DB coverage lives
    //! in `tests/integration/transactions_expressions.rs`.
    //! The tests reach `FieldRef::new` via its `pub(crate)` constructor
    //! expr lives in the same crate, so direct construction is fine.
    //! Column strings satisfy `assert_plain_ident`.

    use super::*;
    use crate::Expr;
    use crate::descriptor::ModelDescriptor;
    use crate::expr::sql::emit_expr;
    use crate::pg::accumulator::SqlAccumulator;
    use crate::query::field::FieldRef;
    use crate::query::portable::SqlEmitContext;

    // Inert local model — mirrors the stub pattern used across the
    // expr / query unit tests. Only `table_name` matters; no CRUD
    // method is called.
    struct Acct;
    impl crate::model::__sealed::Sealed for Acct {}
    #[allow(clippy::manual_async_fn)]
    impl crate::model::Model for Acct {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "accts"
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
    fn single_arm_case_emits_when_then_else() {
        // Case::when(bal < 0, "overdrawn").otherwise("ok") must emit
        // CASE WHEN balance < $1 THEN $2 ELSE $3 END
        // with three binds — one for the 0 literal on the LHS of the
        // comparison, one for each string literal.
        let f: FieldRef<Acct, i64> = FieldRef::new("balance");
        let expr = Case::when(
            f.as_expr().lt(Expr::literal(0i64)),
            Expr::literal("overdrawn".to_string()),
        )
        .otherwise(Expr::literal("ok".to_string()));
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &expr.node, SqlEmitContext::root())
            .expect("case expression should lower to SQL");
        let sql = qb.sql();
        assert_eq!(
            sql.trim(),
            "CASE WHEN balance < $1 THEN $2 ELSE $3 END",
            "got: {sql}"
        );
    }

    #[test]
    fn multi_arm_case_emits_arms_in_order() {
        // Two WHEN arms + ELSE. Assert the literal order is preserved
        // the emitter does not reorder arms, so the first-match
        // semantics are predictable.
        let f: FieldRef<Acct, i64> = FieldRef::new("balance");
        let g: FieldRef<Acct, i64> = FieldRef::new("balance");
        let expr = Case::when(
            f.as_expr().lt(Expr::literal(0i64)),
            Expr::literal("overdrawn".to_string()),
        )
        .when(
            g.as_expr().eq(Expr::literal(0i64)),
            Expr::literal("zero".to_string()),
        )
        .otherwise(Expr::literal("ok".to_string()));
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &expr.node, SqlEmitContext::root())
            .expect("case expression should lower to SQL");
        let sql = qb.sql();
        assert_eq!(
            sql.trim(),
            "CASE WHEN balance < $1 THEN $2 WHEN balance = $3 THEN $4 ELSE $5 END",
            "got: {sql}"
        );
    }
}
