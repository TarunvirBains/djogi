//! INET/CIDR column operator helpers — containment and overlap operators.
//! # What
//! This module defines the leaf payload structs for Postgres INET/CIDR
//! containment operators (`>>`, `<<`, `>>=`, `<=`, `&&`). The actual
//! `DjogiField<M, InetAddr>` methods are in [`crate::query::field`];
//! this module provides the condition payload types that those methods construct.
//! # SQL operators
//! | Operator | SQL Token | Meaning |
//! |----------|-----------|---------|
//! | contains | `>>` | LHS network contains RHS address/network |
//! | contained_by | `<<` | LHS is contained in RHS network |
//! | contains_or_equals | `>>=` | LHS contains RHS or they are equal |
//! | contained_by_or_equals | `<=` | LHS is contained in RHS or they are equal |
//! | overlaps | `&&` | LHS and RHS share at least one address |
//! Postgres allows `inet`, `cidr`, or bare `ipaddr` as the right-hand
//! side for any of these operators; the typed `DjogiField<M, InetAddr>`
//! API accepts all three via the [`IntoInetFilterValue`] trait.

use crate::query::condition::FilterValue;

/// Payload for `col >> $1` (INET contains).
#[derive(Debug, Clone)]
pub struct InetContainsLeaf {
    /// Column name — validated `&'static str` from `DjogiField`.
    pub column: &'static str,
    /// The RHS value to test containment against.
    pub value: FilterValue,
}

/// Payload for `col << $1` (INET contained by).
#[derive(Debug, Clone)]
pub struct InetContainedByLeaf {
    /// Column name — validated `&'static str` from `DjogiField`.
    pub column: &'static str,
    /// The RHS network that must contain the column value.
    pub value: FilterValue,
}

/// Payload for `col >>= $1` (INET contains or equals).
#[derive(Debug, Clone)]
pub struct InetContainsEqLeaf {
    /// Column name — validated `&'static str` from `DjogiField`.
    pub column: &'static str,
    /// The RHS value; true when contained or equal.
    pub value: FilterValue,
}

/// Payload for `col <<= $1` (INET contained by or equals).
#[derive(Debug, Clone)]
pub struct InetContainedByEqLeaf {
    /// Column name — validated `&'static str` from `DjogiField`.
    pub column: &'static str,
    /// The RHS network; true when contained or equal.
    pub value: FilterValue,
}

/// Payload for `col && $1` (INET overlap).
#[derive(Debug, Clone)]
pub struct InetOverlapLeaf {
    /// Column name — validated `&'static str` from `DjogiField`.
    pub column: &'static str,
    /// The RHS network to test overlap with.
    pub value: FilterValue,
}
