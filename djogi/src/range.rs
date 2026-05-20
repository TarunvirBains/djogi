//! Postgres range predicate payloads.
//!
//! `Range<T>` itself lives in [`crate::pg_types`]. This module owns the
//! query-side payloads for PostgreSQL range operators so `query::condition`
//! and `query::field` can share a small typed representation without storing
//! SQL fragments in the condition tree.
//!
//! Range predicates are SQL-only. They are exposed from root model fields
//! through `explicit_pg_predicate()` because Postgres range canonicalization
//! and operator semantics are not portable to Punnu/Rust evaluation.

use crate::query::condition::FilterValue;

/// PostgreSQL range operator used by a [`RangePredicateLeaf`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RangePredicateOp {
    /// `@>`: range contains an element or another range.
    Contains,
    /// `<@`: range is contained by another range.
    ContainedBy,
    /// `&&`: ranges overlap.
    Overlaps,
    /// `<<`: range is strictly left of another range.
    StrictlyLeftOf,
    /// `>>`: range is strictly right of another range.
    StrictlyRightOf,
    /// `&<`: range does not extend right of another range.
    NotExtendsRightOf,
    /// `&>`: range does not extend left of another range.
    NotExtendsLeftOf,
    /// `-|-`: ranges are adjacent.
    AdjacentTo,
}

impl RangePredicateOp {
    /// SQL operator token with surrounding spaces for direct emitter insertion.
    pub(crate) const fn sql_token(self) -> &'static str {
        match self {
            Self::Contains => " @> ",
            Self::ContainedBy => " <@ ",
            Self::Overlaps => " && ",
            Self::StrictlyLeftOf => " << ",
            Self::StrictlyRightOf => " >> ",
            Self::NotExtendsRightOf => " &< ",
            Self::NotExtendsLeftOf => " &> ",
            Self::AdjacentTo => " -|- ",
        }
    }
}

/// Payload for `range_column OP $1`.
#[derive(Debug, Clone)]
pub struct RangePredicateLeaf {
    /// Column name from a trusted [`crate::query::field::FieldRef`].
    pub column: &'static str,
    /// PostgreSQL range operator.
    pub op: RangePredicateOp,
    /// Bound RHS value. For [`RangePredicateOp::Contains`] this may be either
    /// an element value (`T`) or a range value (`Range<T>`); every other
    /// operator carries a range RHS.
    pub value: FilterValue,
}

impl RangePredicateLeaf {
    pub(crate) fn new(column: &'static str, op: RangePredicateOp, value: FilterValue) -> Self {
        Self { column, op, value }
    }
}

mod sealed {
    pub trait Sealed {}

    impl Sealed for i32 {}
    impl Sealed for i64 {}
    impl Sealed for rust_decimal::Decimal {}
    impl Sealed for time::PrimitiveDateTime {}
    impl Sealed for time::OffsetDateTime {}
    impl Sealed for time::Date {}
}

/// Element types supported by Djogi's built-in range predicates.
///
/// Sealed so downstream code cannot pair a `Range<T>` field with a RHS value
/// that has no matching Djogi range bind variant.
pub trait RangeElement: sealed::Sealed + Clone + 'static {
    /// Wrap a typed `Range<Self>` in the matching query bind carrier.
    fn into_range_filter_value(range: crate::Range<Self>) -> FilterValue
    where
        Self: Sized;
}

impl RangeElement for i32 {
    fn into_range_filter_value(range: crate::Range<Self>) -> FilterValue {
        FilterValue::RangeI32(range)
    }
}

impl RangeElement for i64 {
    fn into_range_filter_value(range: crate::Range<Self>) -> FilterValue {
        FilterValue::RangeI64(range)
    }
}

impl RangeElement for rust_decimal::Decimal {
    fn into_range_filter_value(range: crate::Range<Self>) -> FilterValue {
        FilterValue::RangeDecimal(range)
    }
}

impl RangeElement for time::PrimitiveDateTime {
    fn into_range_filter_value(range: crate::Range<Self>) -> FilterValue {
        FilterValue::RangeTimestamp(range)
    }
}

impl RangeElement for time::OffsetDateTime {
    fn into_range_filter_value(range: crate::Range<Self>) -> FilterValue {
        FilterValue::RangeDateTime(range)
    }
}

impl RangeElement for time::Date {
    fn into_range_filter_value(range: crate::Range<Self>) -> FilterValue {
        FilterValue::RangeDate(range)
    }
}
