//! Arithmetic operator overloads on [`Expr<T>`] — gated on a sealed
//! [`Numeric`] trait so only framework-blessed numeric types compose.
//!
//! # What
//!
//! `impl<T: Numeric> std::ops::Add<Expr<T>> for Expr<T>` (and likewise
//! `Sub`, `Mul`, `Div`) — each consumes both operands and returns an
//! `Expr<T>` whose node is the matching [`super::node::ExprNode`]
//! arithmetic variant. [`Numeric`] is sealed via the standard private-
//! supertrait pattern so downstream crates cannot extend the trait;
//! Djogi is the sole arbiter of which types admit SQL arithmetic.
//!
//! # Why sealed?
//!
//! 1. **Avoid accidental impls.** A user adding a newtype `struct
//!    Percent(f64);` should not be able to `impl Numeric for Percent`
//!    and silently enter an arithmetic composition path the emitter
//!    has no rule for. The sealed pattern blocks the trait at the
//!    crate boundary.
//! 2. **Framework controls the bind surface.** Adding `Decimal` or
//!    `Interval` to `Numeric` must happen alongside the matching
//!    [`super::node::ExprNode`] bind wiring and the [`FilterValue`
//!    ][crate::query::condition::FilterValue] variant. Sealing keeps
//!    those two sides in lockstep.
//! 3. **Forward-compat for Phase 5.** The plan explicitly reserves
//!    `Decimal` for Phase 5 and `Interval` for the interval-arithmetic
//!    milestone. Phase 4 ships the integer + float subset only; sealing
//!    means adding the missing types later is additive (one new impl)
//!    rather than breaking (an existing blanket impl would already
//!    admit them).
//!
//! # Why the operator overloads and not named `.add(..)` methods?
//!
//! The ergonomic target is Django-style:
//!
//! ```ignore
//! .update_expr(|f| f.view_count.set_expr(f.view_count.as_expr() + 1))
//! ```
//!
//! — `+` reads as SQL `+`. Named methods would force every arithmetic
//! site to be an explicit function call, which buries the
//! composition. `std::ops::Add` is the idiomatic choice; the sealed
//! `Numeric` bound keeps it safe.

use crate::expr::Expr;
use crate::expr::node::ExprNode;

/// Sealed marker trait — only Djogi-blessed numeric types implement
/// this. Phase 4 ships integer + float; Phase 5 extends with `Decimal`;
/// the interval-arithmetic milestone extends again with `Interval`.
///
/// Crate-private supertrait `sealed::Sealed` is the seal — downstream
/// code cannot name `sealed::Sealed`, so `impl Numeric for MyType {}`
/// fails at "the trait `Sealed` is not implemented for `MyType`".
pub trait Numeric: sealed::Sealed {}

mod sealed {
    /// The seal. Not reachable from outside the crate.
    pub trait Sealed {}

    impl Sealed for i16 {}
    impl Sealed for i32 {}
    impl Sealed for i64 {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}
}

impl Numeric for i16 {}
impl Numeric for i32 {}
impl Numeric for i64 {}
impl Numeric for f32 {}
impl Numeric for f64 {}

// The operator impls are one-per-op because `std::ops::Add` /
// `::Sub` / `::Mul` / `::Div` are separate traits. A macro would hide
// the boilerplate but also the place where a future reviewer would
// look to audit "which arithmetic forms does the emitter support?".
// Explicit impls make that audit a grep for `impl Add for Expr`.

impl<T: Numeric> std::ops::Add for Expr<T> {
    type Output = Expr<T>;

    fn add(self, rhs: Self) -> Self::Output {
        Expr::from_node(ExprNode::Add(Box::new(self.node), Box::new(rhs.node)))
    }
}

impl<T: Numeric> std::ops::Sub for Expr<T> {
    type Output = Expr<T>;

    fn sub(self, rhs: Self) -> Self::Output {
        Expr::from_node(ExprNode::Sub(Box::new(self.node), Box::new(rhs.node)))
    }
}

impl<T: Numeric> std::ops::Mul for Expr<T> {
    type Output = Expr<T>;

    fn mul(self, rhs: Self) -> Self::Output {
        Expr::from_node(ExprNode::Mul(Box::new(self.node), Box::new(rhs.node)))
    }
}

impl<T: Numeric> std::ops::Div for Expr<T> {
    type Output = Expr<T>;

    fn div(self, rhs: Self) -> Self::Output {
        Expr::from_node(ExprNode::Div(Box::new(self.node), Box::new(rhs.node)))
    }
}
