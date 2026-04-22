//! Literal bridges — `impl From<V> for Expr<V>` for every SQL-bindable
//! scalar Djogi ships with.
//!
//! # What
//!
//! Each scalar `V` that Djogi knows how to bind to a Postgres parameter
//! (every variant of [`FilterValue`] that carries exactly one value) gets
//! one `impl From<V> for Expr<V>` here. This is the typed promotion path:
//! `Expr::literal(100i32)` ends up as `Expr<i32>` carrying
//! `ExprNode::Literal(FilterValue::I32(100))`, and composition methods
//! (`eq`, `neq`, arithmetic operators) can rely on `V` matching the
//! column's declared type at the `FieldRef<M, V>` side.
//!
//! # Why mirror [`crate::query::field::IntoFilterValue`] rather than
//! composing with it?
//!
//! `IntoFilterValue` is the Phase 2 bridge from a Rust value to a
//! `FilterValue`. We *could* implement `impl<V: IntoFilterValue> From<V>
//! for Expr<V>` and inherit every bindable type for free, but that
//! blanket impl would collide with the reflexive `From<T> for T` if we
//! ever add a public-facing `From<Expr<T>>` wrapper later (e.g. for
//! `set_expr` ergonomics in Task 3b). Writing the impls out by hand is
//! repetitive but additive-safe — Phase 5's `Decimal` / `Interval`
//! extensions slot in without touching existing impls, and no blanket
//! impl will ever conflict with a hypothetical `impl From<Expr<T>> for
//! Expr<T>` or similar.
//!
//! # Coverage (must match [`FilterValue`] one-for-one)
//!
//! - `String` / `&'static str` — the `&str` case maps into `Expr<String>`
//!   so literals like `Expr::literal("draft")` don't leave the user
//!   with an unbindable `&'static str` expression (Postgres has no
//!   `&str`, only `TEXT`).
//! - `i16 / i32 / i64 / f32 / f64 / bool` — scalar numerics.
//! - `time::OffsetDateTime` / `time::Date` — the Djogi canonical
//!   timestamp / date types (re-exported as `DateTime` / `Date`).
//! - `uuid::Uuid` / `HeerId` / `RanjId` — id types.
//!
//! The intentionally-omitted `FilterValue::Null / ::List / ::Pair`
//! variants do not get `From` impls here: null is not a typed value
//! (there is no `Expr<NULL>`), lists belong in `IN (...)` which is a
//! separate `FieldRef` lookup, and pairs are the `BETWEEN` payload
//! shape, not a standalone expression.

use crate::expr::Expr;
use crate::query::condition::FilterValue;
use crate::{HeerId, RanjId};

impl From<String> for Expr<String> {
    fn from(v: String) -> Self {
        Self::from_literal(FilterValue::String(v))
    }
}

// `&'static str` maps into `Expr<String>` — the column type is always
// `TEXT` / `VARCHAR` on the Postgres side, so the expression's Rust
// tag type is `String`, matching how [`FilterValue::String`] carries
// an owned `String` after binding. Non-`'static` references would not
// round-trip through `ExprNode` cleanly (the node tree is `'static`);
// callers with borrowed strings should `.to_owned()` first.
impl From<&'static str> for Expr<String> {
    fn from(v: &'static str) -> Self {
        Self::from_literal(FilterValue::String(v.to_owned()))
    }
}

impl From<i16> for Expr<i16> {
    fn from(v: i16) -> Self {
        Self::from_literal(FilterValue::I16(v))
    }
}

impl From<i32> for Expr<i32> {
    fn from(v: i32) -> Self {
        Self::from_literal(FilterValue::I32(v))
    }
}

impl From<i64> for Expr<i64> {
    fn from(v: i64) -> Self {
        Self::from_literal(FilterValue::I64(v))
    }
}

impl From<f32> for Expr<f32> {
    fn from(v: f32) -> Self {
        Self::from_literal(FilterValue::F32(v))
    }
}

impl From<f64> for Expr<f64> {
    fn from(v: f64) -> Self {
        Self::from_literal(FilterValue::F64(v))
    }
}

impl From<bool> for Expr<bool> {
    fn from(v: bool) -> Self {
        Self::from_literal(FilterValue::Bool(v))
    }
}

impl From<time::OffsetDateTime> for Expr<time::OffsetDateTime> {
    fn from(v: time::OffsetDateTime) -> Self {
        Self::from_literal(FilterValue::DateTime(v))
    }
}

impl From<time::Date> for Expr<time::Date> {
    fn from(v: time::Date) -> Self {
        Self::from_literal(FilterValue::Date(v))
    }
}

impl From<uuid::Uuid> for Expr<uuid::Uuid> {
    fn from(v: uuid::Uuid) -> Self {
        Self::from_literal(FilterValue::Uuid(v))
    }
}

impl From<HeerId> for Expr<HeerId> {
    fn from(v: HeerId) -> Self {
        Self::from_literal(FilterValue::HeerId(v))
    }
}

impl From<RanjId> for Expr<RanjId> {
    fn from(v: RanjId) -> Self {
        Self::from_literal(FilterValue::RanjId(v))
    }
}

// `time::Duration` maps to `ExprNode::IntervalLiteral` rather than
// `FilterValue::*` because `tokio-postgres` / `postgres-types` does not ship
// a `ToSql` impl for `time::Duration`. Instead of adding a bind parameter,
// the emitter renders the duration as a raw SQL token —
// `INTERVAL '{microseconds} microseconds'` — so the value is embedded
// directly in the query text.
//
// `Expr::literal(time::Duration::days(30))` produces an `Expr<time::Duration>`
// whose node is `ExprNode::IntervalLiteral { microseconds: 2592000000000 }`.
// This expression composes with `Expr<time::OffsetDateTime>` arithmetic via the
// heterogeneous `Add<Expr<Duration>> for Expr<OffsetDateTime>` impl in
// `expr::arithmetic`.
impl From<time::Duration> for crate::expr::Expr<time::Duration> {
    fn from(d: time::Duration) -> Self {
        use crate::expr::node::ExprNode;
        let microseconds = saturating_micros(d);
        crate::expr::Expr::from_node(ExprNode::IntervalLiteral { microseconds })
    }
}

/// Convert a `time::Duration` to microseconds as `i64`, saturating at the
/// boundaries of `i64` range rather than wrapping.
///
/// `time::Duration::whole_microseconds()` returns `i128`. For Durations within
/// `i64`'s microsecond range (roughly ±292,277 years) the result is exact.
/// For larger Durations (e.g. `time::Duration::MAX`), the value saturates to
/// `i64::MAX` or `i64::MIN` rather than wrapping silently via an `as i64` cast.
/// Saturation is the correct behaviour here: a Duration beyond ±292 millennia
/// is already a Postgres `INTERVAL` overflow, and the saturated value correctly
/// signals "maximum expressible" to the DB rather than producing a bogus
/// wrapped value.
///
/// Exposed as a `pub(crate)` free function so unit tests can call it directly.
pub(crate) fn saturating_micros(d: time::Duration) -> i64 {
    let raw: i128 = d.whole_microseconds();
    raw.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

#[cfg(test)]
mod tests {
    use super::saturating_micros;
    use crate::expr::node::ExprNode;

    #[test]
    fn duration_literal_saturates_max() {
        // time::Duration::MAX as microseconds overflows i64 — must saturate to i64::MAX.
        let micros = saturating_micros(time::Duration::MAX);
        assert_eq!(
            micros,
            i64::MAX,
            "Duration::MAX must saturate to i64::MAX, not wrap"
        );
        // Verify ExprNode is constructed with the saturated value.
        let node = ExprNode::IntervalLiteral {
            microseconds: micros,
        };
        if let ExprNode::IntervalLiteral { microseconds } = node {
            assert_eq!(microseconds, i64::MAX);
        } else {
            panic!("expected IntervalLiteral");
        }
    }

    #[test]
    fn duration_literal_saturates_min() {
        // The most-negative Duration also overflows i64 — must saturate to i64::MIN.
        let micros = saturating_micros(time::Duration::MIN);
        assert_eq!(
            micros,
            i64::MIN,
            "Duration::MIN must saturate to i64::MIN, not wrap"
        );
        let node = ExprNode::IntervalLiteral {
            microseconds: micros,
        };
        if let ExprNode::IntervalLiteral { microseconds } = node {
            assert_eq!(microseconds, i64::MIN);
        } else {
            panic!("expected IntervalLiteral");
        }
    }

    #[test]
    fn duration_literal_in_range_is_exact() {
        // 30 days in microseconds — well within i64 range; must be exact.
        let d = time::Duration::days(30);
        let expected_micros: i64 = 30 * 24 * 60 * 60 * 1_000_000;
        let micros = saturating_micros(d);
        assert_eq!(
            micros, expected_micros,
            "in-range Duration must produce exact microsecond count"
        );
    }
}
