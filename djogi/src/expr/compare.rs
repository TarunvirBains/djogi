//! Comparison methods on [`Expr<T>`] — the `Expr<bool>`-producing operations.
//!
//! # What
//!
//! Six methods — `eq`, `neq`, `gt`, `gte`, `lt`, `lte` — each consuming
//! `self` and a right-hand-side operand that implements `Into<Expr<T>>`
//! (so literals, other expressions, and field refs all interop). Each
//! returns `Expr<bool>` by wrapping the two operands in
//! [`crate::expr::node::ExprNode::Cmp`] with the matching [`CmpOp`].
//!
//! # Why `Into<Expr<T>>` on the RHS?
//!
//! The ergonomic target is:
//!
//! ```ignore
//! f.balance().as_expr().lt(100i64)                // literal RHS
//! f.balance().as_expr().lt(f.overdraft_limit().as_expr())  // field RHS
//! f.balance().as_expr().lt(f.balance().as_expr() + Expr::literal(10))  // arithmetic RHS
//! ```
//!
//! All three RHS forms are `Into<Expr<i64>>`: literals via the
//! [`super::literal`] impls, `Expr<T>` via the reflexive `From<T> for T`,
//! and arithmetic expressions because [`super::arithmetic`] returns
//! `Expr<T>`. One generic bound covers every call site; no method
//! overloads needed.
//!
//! # Why `Expr<bool>` and not `Condition`?
//!
//! The Phase 2 `FieldRef::eq` returns [`crate::query::condition::Condition`]
//! directly — that surface is for literal-RHS filters built in closures.
//! The expression IR is the path for field-vs-field / arithmetic /
//! subquery predicates, so it stays on the `Expr<bool>` type until the
//! promotion point in [`crate::query::QuerySet::filter_expr`] wraps it
//! in `Condition::Expr`. Keeping the types distinct means the type
//! system can flag "you passed an `Expr<bool>` where a `Condition` was
//! expected" (and vice versa) instead of silently round-tripping.

use crate::expr::Expr;
use crate::expr::node::{CmpOp, ExprNode};

impl<T> Expr<T> {
    /// Build an `Expr<bool>` for `<lhs> <op> <rhs>` — used by every
    /// public comparison method below.
    fn cmp<R>(self, op: CmpOp, rhs: R) -> Expr<bool>
    where
        R: Into<Expr<T>>,
    {
        let rhs = rhs.into();
        Expr::from_node(ExprNode::Cmp {
            op,
            lhs: Box::new(self.node),
            rhs: Box::new(rhs.node),
        })
    }

    /// `lhs = rhs` — SQL equality as an expression.
    ///
    /// The returned `Expr<bool>` slots into
    /// [`crate::query::QuerySet::filter_expr`] via the
    /// [`crate::query::condition::Condition::Expr`] bridge, or into
    /// nested boolean expressions composed via the typed `Expr<bool>`
    /// surface.
    pub fn eq<R: Into<Expr<T>>>(self, rhs: R) -> Expr<bool> {
        self.cmp(CmpOp::Eq, rhs)
    }

    /// `lhs <> rhs` — SQL inequality as an expression.
    pub fn neq<R: Into<Expr<T>>>(self, rhs: R) -> Expr<bool> {
        self.cmp(CmpOp::Neq, rhs)
    }

    /// `lhs > rhs`.
    pub fn gt<R: Into<Expr<T>>>(self, rhs: R) -> Expr<bool> {
        self.cmp(CmpOp::Gt, rhs)
    }

    /// `lhs >= rhs`.
    pub fn gte<R: Into<Expr<T>>>(self, rhs: R) -> Expr<bool> {
        self.cmp(CmpOp::Gte, rhs)
    }

    /// `lhs < rhs`.
    pub fn lt<R: Into<Expr<T>>>(self, rhs: R) -> Expr<bool> {
        self.cmp(CmpOp::Lt, rhs)
    }

    /// `lhs <= rhs`.
    pub fn lte<R: Into<Expr<T>>>(self, rhs: R) -> Expr<bool> {
        self.cmp(CmpOp::Lte, rhs)
    }
}
