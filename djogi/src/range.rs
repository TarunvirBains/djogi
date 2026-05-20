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
///
/// Fields are `pub(crate)` so the only construction path is through the
/// typed range predicate methods on `FieldRef`. Downstream code can inspect
/// range predicates via the read-only accessors but cannot forge a raw column
/// string into a public [`crate::query::condition::Condition`] variant.
#[derive(Debug, Clone)]
pub struct RangePredicateLeaf {
    /// Column name from a trusted [`crate::query::field::FieldRef`].
    pub(crate) column: &'static str,
    /// PostgreSQL range operator.
    pub(crate) op: RangePredicateOp,
    /// Bound RHS value. For [`RangePredicateOp::Contains`] this may be either
    /// an element value (`T`) or a range value (`Range<T>`); every other
    /// operator carries a range RHS.
    pub(crate) value: FilterValue,
    /// Explicit scalar cast for `contains(element)` binds. Range-vs-range
    /// predicates leave this `None` and keep the existing typed range bind.
    pub(crate) rhs_element_cast: Option<&'static str>,
}

impl RangePredicateLeaf {
    pub(crate) fn new(column: &'static str, op: RangePredicateOp, value: FilterValue) -> Self {
        Self {
            column,
            op,
            value,
            rhs_element_cast: None,
        }
    }

    pub(crate) fn with_rhs_element_cast(mut self, cast: &'static str) -> Self {
        self.rhs_element_cast = Some(cast);
        self
    }

    /// Column name from the trusted field reference that built this predicate.
    pub fn column(&self) -> &'static str {
        self.column
    }

    /// PostgreSQL range operator for this predicate.
    pub fn op(&self) -> RangePredicateOp {
        self.op
    }

    /// Bound right-hand-side value for this predicate.
    pub fn value(&self) -> &FilterValue {
        &self.value
    }

    /// Explicit scalar cast for `contains(element)` binds, if present.
    pub fn rhs_element_cast(&self) -> Option<&'static str> {
        self.rhs_element_cast
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

    /// Postgres scalar subtype used when `range @> element` needs an explicit
    /// RHS cast to select the element overload.
    fn sql_element_cast() -> &'static str
    where
        Self: Sized;
}

impl RangeElement for i32 {
    fn into_range_filter_value(range: crate::Range<Self>) -> FilterValue {
        FilterValue::RangeI32(range)
    }

    fn sql_element_cast() -> &'static str {
        "int4"
    }
}

impl RangeElement for i64 {
    fn into_range_filter_value(range: crate::Range<Self>) -> FilterValue {
        FilterValue::RangeI64(range)
    }

    fn sql_element_cast() -> &'static str {
        "int8"
    }
}

impl RangeElement for rust_decimal::Decimal {
    fn into_range_filter_value(range: crate::Range<Self>) -> FilterValue {
        FilterValue::RangeDecimal(range)
    }

    fn sql_element_cast() -> &'static str {
        "numeric"
    }
}

impl RangeElement for time::PrimitiveDateTime {
    fn into_range_filter_value(range: crate::Range<Self>) -> FilterValue {
        FilterValue::RangeTimestamp(range)
    }

    fn sql_element_cast() -> &'static str {
        "timestamp"
    }
}

impl RangeElement for time::OffsetDateTime {
    fn into_range_filter_value(range: crate::Range<Self>) -> FilterValue {
        FilterValue::RangeDateTime(range)
    }

    fn sql_element_cast() -> &'static str {
        "timestamptz"
    }
}

impl RangeElement for time::Date {
    fn into_range_filter_value(range: crate::Range<Self>) -> FilterValue {
        FilterValue::RangeDate(range)
    }

    fn sql_element_cast() -> &'static str {
        "date"
    }
}
