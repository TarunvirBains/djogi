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
use time::{Duration, OffsetDateTime};

/// Sealed marker trait — only Djogi-blessed numeric types implement
/// this. Phase 4 ships integer + float; Phase 5 extends with `Decimal`;
/// the interval-arithmetic milestone extends again with `Interval`.
///
/// Crate-private supertrait `sealed::Sealed` is the seal — downstream
/// code cannot name `sealed::Sealed`, so `impl Numeric for MyType {}`
/// fails at "the trait `Sealed` is not implemented for `MyType`".
///
/// # Sum / avg cast targets
///
/// Each `Numeric` impl carries a pair of associated constants —
/// [`Numeric::SUM_CAST`] and [`Numeric::AVG_CAST`] — that name the
/// Postgres type the aggregate result must be cast to so it decodes
/// back into the Rust type the typed
/// [`super::aggregate::AggregateExpr<Out>`] surface promised.
///
/// Why they exist: Postgres widens integer aggregates. `SUM(BIGINT)`
/// returns `NUMERIC`; `AVG(BIGINT)` returns `NUMERIC`; `SUM(SMALLINT)`
/// returns `BIGINT`. The typed wrapper's `Out = V` / `Out = f64`
/// promise only holds if we emit an explicit `::<pg_type>` narrowing
/// cast. Stamping the target as an associated `&'static str` per
/// `Numeric` impl keeps the aggregate builder trivial — one
/// `V::SUM_CAST` / `V::AVG_CAST` lookup, no per-type switch on the
/// method side.
pub trait Numeric: sealed::Sealed {
    /// Postgres type to cast `SUM(col)` back to so the decoder returns
    /// `V` directly. Always a framework-baked `&'static str`
    /// (`"BIGINT"` for `i64`, `"REAL"` for `f32`, etc.) — never user
    /// input, safe to push as a raw SQL token.
    const SUM_CAST: &'static str;
    /// Postgres type to cast `AVG(col)` back to so the decoder returns
    /// `f64` directly. Always `"DOUBLE PRECISION"` for Phase 4's
    /// numeric set — `AVG` always returns `f64` at the typed surface.
    const AVG_CAST: &'static str;
}

mod sealed {
    /// The seal. Not reachable from outside the crate.
    pub trait Sealed {}

    impl Sealed for i16 {}
    impl Sealed for i32 {}
    impl Sealed for i64 {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}
    impl Sealed for time::Duration {}
}

// Per-type cast targets. Integer sums widen in Postgres
// (SMALLINT -> BIGINT, BIGINT -> NUMERIC); we narrow back to the input
// type so the typed `Out = V` decode path works. Floats don't widen on
// SUM, but we still emit the cast for uniformity — Postgres treats
// `REAL::REAL` and `DOUBLE PRECISION::DOUBLE PRECISION` as no-ops, so
// the round-trip SQL is semantically identical and the emitter stays
// one code path across all Numeric types.
//
// AVG always returns NUMERIC for integer inputs and DOUBLE PRECISION
// for floating-point inputs; casting to DOUBLE PRECISION lets us
// decode into `f64` uniformly, which matches the typed surface's
// `Out = f64` promise on `avg()`.

// Per-type cast target matches the Rust type `Out = V` promises:
// `i16` maps to Postgres `SMALLINT`, `i32` to `INTEGER`, `i64`
// to `BIGINT`, `f32` to `REAL`, `f64` to `DOUBLE PRECISION`. Narrowing
// casts from a wider Postgres type (e.g. `NUMERIC -> BIGINT`) raise a
// `numeric_value_out_of_range` error if the sum genuinely overflows
// the target, which is the correct behaviour — users aggregating values
// that wouldn't fit back into `V` should declare a larger `V` on the
// column or use `ctx.raw_scalar`. This is documented on `sum()` below.
impl Numeric for i16 {
    const SUM_CAST: &'static str = "SMALLINT";
    const AVG_CAST: &'static str = "DOUBLE PRECISION";
}
impl Numeric for i32 {
    const SUM_CAST: &'static str = "INTEGER";
    const AVG_CAST: &'static str = "DOUBLE PRECISION";
}
impl Numeric for i64 {
    const SUM_CAST: &'static str = "BIGINT";
    const AVG_CAST: &'static str = "DOUBLE PRECISION";
}
impl Numeric for f32 {
    const SUM_CAST: &'static str = "REAL";
    const AVG_CAST: &'static str = "DOUBLE PRECISION";
}
impl Numeric for f64 {
    const SUM_CAST: &'static str = "DOUBLE PRECISION";
    const AVG_CAST: &'static str = "DOUBLE PRECISION";
}

// `time::Duration` is the Rust-side representation of a Postgres `INTERVAL`.
// Adding it to `Numeric` lets `Expr<Duration> + Expr<Duration>` (interval +
// interval), `Expr<Duration> * Expr<i64>` (when mixed-type Mul lands), and
// most importantly `Expr<Duration>` to act as the RHS in the heterogeneous
// `Expr<OffsetDateTime> + Expr<Duration>` impl below.
//
// SUM_CAST / AVG_CAST: Postgres `SUM(INTERVAL)` returns `INTERVAL` (no
// widening), and `AVG(INTERVAL)` returns `INTERVAL`. The cast targets match
// the Postgres names for INTERVAL to satisfy the trait contract, but they are
// only used by the aggregate terminal's `emit_aggregate_with_cast` path, which
// is rarely exercised for INTERVAL columns. Any caller who needs precision
// INTERVAL aggregation should use `ctx.raw_scalar` until a typed aggregate
// path lands.
impl Numeric for time::Duration {
    const SUM_CAST: &'static str = "INTERVAL";
    const AVG_CAST: &'static str = "INTERVAL";
}

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

// ── Interval +/- interval arithmetic ────────────────────────────────────
//
// `crate::Interval` is `djogi::Interval` (a three-component
// month/day/microsecond representation). SQL supports interval
// arithmetic natively, and callers now need `Expr<crate::Interval>`
// to compose `col + $interval` / `col - $interval` update expressions
// through narrow impls.
impl std::ops::Add for Expr<crate::Interval> {
    type Output = Expr<crate::Interval>;

    fn add(self, rhs: Self) -> Self::Output {
        Expr::from_node(ExprNode::Add(Box::new(self.node), Box::new(rhs.node)))
    }
}

impl std::ops::Sub for Expr<crate::Interval> {
    type Output = Expr<crate::Interval>;

    fn sub(self, rhs: Self) -> Self::Output {
        Expr::from_node(ExprNode::Sub(Box::new(self.node), Box::new(rhs.node)))
    }
}

// ── Heterogeneous datetime + interval arithmetic ───────────────────────────
//
// `Expr<OffsetDateTime> + Expr<Duration>` → `Expr<OffsetDateTime>`.
//
// Postgres supports `TIMESTAMPTZ + INTERVAL` natively, producing a
// `TIMESTAMPTZ` result. The typed `Add` impl above only covers
// `Expr<T> + Expr<T>` where `T: Numeric` (same-type addition). The
// DateTime + Interval combination is fundamentally heterogeneous: the two
// operands have different types. A dedicated impl here keeps the
// crate-private `Add` seal intact — downstream crates cannot add new
// numeric types — while still covering the idiomatic date-arithmetic shape.
//
// `Expr<OffsetDateTime> - Expr<Duration>` is the corresponding subtraction.

impl std::ops::Add<Expr<Duration>> for Expr<OffsetDateTime> {
    type Output = Expr<OffsetDateTime>;

    fn add(self, rhs: Expr<Duration>) -> Self::Output {
        Expr::from_node(ExprNode::Add(Box::new(self.node), Box::new(rhs.node)))
    }
}

impl std::ops::Sub<Expr<Duration>> for Expr<OffsetDateTime> {
    type Output = Expr<OffsetDateTime>;

    fn sub(self, rhs: Expr<Duration>) -> Self::Output {
        Expr::from_node(ExprNode::Sub(Box::new(self.node), Box::new(rhs.node)))
    }
}
