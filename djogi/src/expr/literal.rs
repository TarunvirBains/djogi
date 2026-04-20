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
