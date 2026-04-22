//! Array column operator helpers — `contains`, `contained_by`, `overlap`, `len`.
//!
//! # What
//!
//! This module defines the condition variants for Postgres array operators and
//! the `len()` expression. The actual `FieldRef<M, Vec<V>>` methods are in
//! [`crate::query::field`]; this module provides the condition payload types
//! that those methods construct.
//!
//! # SQL operators
//!
//! | Method         | SQL                      | Meaning                                |
//! |----------------|--------------------------|----------------------------------------|
//! | `contains`     | `col @> $1`              | column contains all values in the array |
//! | `contained_by` | `col <@ $1`              | column values are all in the argument   |
//! | `overlap`      | `col && $1`              | column and argument share at least one element |
//! | `len`          | `array_length(col, 1)`   | number of elements (1-dimensional arrays only) |
//!
//! Djogi arrays are always 1-dimensional; the `array_length` call is hardcoded
//! to dimension 1. Multi-dimensional arrays are not a supported field type.
//!
//! # No GIN index emission
//!
//! These operators benefit from GIN indexes, but GIN index emission is
//! deferred to Phase 7. The operators work correctly without an index — Postgres
//! falls back to a sequential scan.

use crate::query::condition::FilterValue;

/// Payload for `col @> $1` (array contains).
///
/// The `values` vector is the RHS array to bind as a Postgres array parameter.
/// Every element must be the same Rust type as the column's element type
/// (enforced at construction time by the typed `FieldRef<M, Vec<V>>` API).
#[derive(Debug, Clone)]
pub struct ArrayContainsLeaf {
    /// Column name — validated `&'static str` from `FieldRef`.
    pub column: &'static str,
    /// The array argument to test containment against.
    pub values: FilterValue,
}

/// Payload for `col <@ $1` (array contained by).
#[derive(Debug, Clone)]
pub struct ArrayContainedByLeaf {
    /// Column name — validated `&'static str` from `FieldRef`.
    pub column: &'static str,
    /// The array argument that `column` must be a subset of.
    pub values: FilterValue,
}

/// Payload for `col && $1` (array overlap — at least one element in common).
#[derive(Debug, Clone)]
pub struct ArrayOverlapLeaf {
    /// Column name — validated `&'static str` from `FieldRef`.
    pub column: &'static str,
    /// The array argument to test overlap with.
    pub values: FilterValue,
}
