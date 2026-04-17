//! Condition tree — the filter AST that `QuerySet<T>` accumulates.
//!
//! `Condition` is deliberately shallow: no SQL strings are stored here. SQL
//! generation happens exactly once at terminal-method time (`query::sql`).
//! This keeps composition cheap (cloning a QuerySet = cloning a Vec<Condition>
//! — no duplicated SQL buffers) and keeps the injection-safety review surface
//! in one place.

use crate::HeerId;
use crate::RanjId;

/// A filter condition. Built by `FieldRef` lookup methods; composed via
/// `.and()` / `.or()` / `Condition::not`.
#[derive(Debug, Clone)]
pub enum Condition {
    /// Vacuous — the root state before any filter applies. `ConditionBuilder`
    /// emits nothing for `True`, so a QuerySet with no filters produces a
    /// `SELECT ...` without a `WHERE` clause.
    True,
    Leaf(Leaf),
    And(Vec<Condition>),
    Or(Vec<Condition>),
    Not(Box<Condition>),
}

// Written out explicitly instead of `#[derive(Default)]` + `#[default]` on the
// variant: the enum's doc comments are load-bearing (they explain why `True`
// is the vacuous root), and reshuffling them around a derive attribute hurts
// readability. The clippy hint is correct but optimizes the wrong axis here.
#[allow(clippy::derivable_impls)]
impl Default for Condition {
    fn default() -> Self {
        Condition::True
    }
}

impl Condition {
    /// Combine two conditions with SQL AND. Flattens nested `And` trees to
    /// keep the structure shallow for the emitter.
    pub fn and(a: Condition, b: Condition) -> Condition {
        match (a, b) {
            (Condition::True, c) | (c, Condition::True) => c,
            (Condition::And(mut va), Condition::And(vb)) => {
                va.extend(vb);
                Condition::And(va)
            }
            (Condition::And(mut va), other) => {
                va.push(other);
                Condition::And(va)
            }
            (other, Condition::And(vb)) => {
                let mut v = vec![other];
                v.extend(vb);
                Condition::And(v)
            }
            (a, b) => Condition::And(vec![a, b]),
        }
    }

    /// Combine two conditions with SQL OR. Flattens nested `Or` trees.
    pub fn or(a: Condition, b: Condition) -> Condition {
        match (a, b) {
            (Condition::Or(mut va), Condition::Or(vb)) => {
                va.extend(vb);
                Condition::Or(va)
            }
            (Condition::Or(mut va), other) => {
                va.push(other);
                Condition::Or(va)
            }
            (other, Condition::Or(vb)) => {
                let mut v = vec![other];
                v.extend(vb);
                Condition::Or(v)
            }
            (a, b) => Condition::Or(vec![a, b]),
        }
    }

    /// Wrap a condition in SQL NOT. `Not(Not(x)) == x` is NOT auto-simplified
    /// — users rarely write double-negation by accident and the emitter can
    /// handle it correctly either way.
    ///
    /// Deliberately an associated function (not `impl std::ops::Not`) so the
    /// call-site reads `Condition::not(cond)` matching `Condition::and(a, b)`
    /// / `Condition::or(a, b)`. A unary `!cond` operator would be terser but
    /// would split the combinator API across two idioms.
    #[allow(clippy::should_implement_trait)]
    pub fn not(inner: Condition) -> Condition {
        Condition::Not(Box::new(inner))
    }
}

/// A single column-level comparison: `column op value`.
#[derive(Debug, Clone)]
pub struct Leaf {
    pub column: &'static str,
    pub op: LookupOp,
    pub value: FilterValue,
}

impl Leaf {
    /// Test helper — not public API. Phase 2 users construct leaves via
    /// `FieldRef` lookup methods.
    #[doc(hidden)]
    pub fn eq_raw(column: &'static str, value: FilterValue) -> Leaf {
        Leaf {
            column,
            op: LookupOp::Eq,
            value,
        }
    }
}

/// The operator half of a `Leaf`. Every Phase 2 lookup method maps to one
/// of these variants. SQL emission (`query::sql`) pattern-matches on this
/// enum to produce the correct operator token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupOp {
    Eq,
    Neq,
    Gt,
    Gte,
    Lt,
    Lte,
    In,
    NotIn,
    IsNull,
    IsNotNull,
    /// ILIKE '%s%' — spec §5.4 `contains`.
    IContains,
    /// ILIKE 's%' — spec §5.4 `starts_with`.
    IStartsWith,
    /// ILIKE '%s' — spec §5.4 `ends_with`.
    IEndsWith,
    /// BETWEEN a AND b.
    Between,
    /// Case-insensitive equality via `LOWER(col) = LOWER($n)`.
    IExact,
    /// POSIX regex (`~`).
    Regex,
    /// Case-insensitive POSIX regex (`~*`).
    IRegex,
}

/// A concrete value to bind to a query parameter. One variant per
/// SQL-bindable Rust type Djogi knows about. Implementers adding a new
/// column type (e.g. `Decimal` in Phase 5) extend this enum here; `sql.rs`
/// pattern-matches and calls `qb.push_bind(v)` on each variant.
///
/// `List` carries a boxed `Vec<FilterValue>` for `IN (...)` / `NOT IN (...)`.
/// Mixed-type lists are representable but the emitter rejects them — the
/// typed `FieldRef<M, V>` API prevents construction from user code.
#[derive(Debug, Clone)]
pub enum FilterValue {
    String(String),
    I16(i16),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    Bool(bool),
    DateTime(time::OffsetDateTime),
    Date(time::Date),
    Uuid(uuid::Uuid),
    HeerId(HeerId),
    RanjId(RanjId),
    Null,
    /// For IN (...) / NOT IN (...) list lookups.
    List(Vec<FilterValue>),
    /// BETWEEN a AND b payload (two bound values).
    Pair(Box<FilterValue>, Box<FilterValue>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn condition_true_is_default() {
        let c = Condition::default();
        assert!(matches!(c, Condition::True));
    }

    #[test]
    fn and_flattens_nested_ands() {
        let a = Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true)));
        let b = Condition::Leaf(Leaf::eq_raw("b", FilterValue::Bool(false)));
        let c = Condition::Leaf(Leaf::eq_raw("c", FilterValue::Bool(true)));
        let combined = Condition::and(Condition::and(a, b), c);
        if let Condition::And(parts) = combined {
            assert_eq!(
                parts.len(),
                3,
                "nested And should flatten to 3 leaves, got {parts:?}"
            );
        } else {
            panic!("expected And, got {combined:?}");
        }
    }

    #[test]
    fn and_with_true_is_identity() {
        let leaf = Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true)));
        let combined = Condition::and(Condition::True, leaf.clone());
        assert!(matches!(combined, Condition::Leaf(_)));
    }

    #[test]
    fn or_flattens_nested_ors() {
        let a = Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true)));
        let b = Condition::Leaf(Leaf::eq_raw("b", FilterValue::Bool(false)));
        let c = Condition::Leaf(Leaf::eq_raw("c", FilterValue::Bool(true)));
        let combined = Condition::or(Condition::or(a, b), c);
        if let Condition::Or(parts) = combined {
            assert_eq!(parts.len(), 3);
        } else {
            panic!("expected Or");
        }
    }
}
