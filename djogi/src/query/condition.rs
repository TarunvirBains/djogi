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

    /// SQL `(a AND b AND c)`. An empty `And(vec![])` is the vacuous-truth
    /// identity — Task 6's emitter renders it as `TRUE`. `and()` never
    /// constructs an empty vector from its own inputs, but public API
    /// consumers technically can; the invariant is "empty = TRUE".
    And(Vec<Condition>),

    /// SQL `(a OR b OR c)`. An empty `Or(vec![])` is the vacuous-falsehood
    /// identity — Task 6's emitter renders it as `FALSE`. `or()` never
    /// constructs an empty vector from its own inputs.
    Or(Vec<Condition>),

    Not(Box<Condition>),

    /// Bridge from the typed expression IR (Phase 4 Task 3a) — an
    /// `Expr<bool>` slotted into the filter tree by
    /// [`crate::query::QuerySet::filter_expr`]. The SQL emitter
    /// delegates to [`crate::expr::sql::emit_expr`] for this variant,
    /// bypassing [`super::sql::emit_leaf`] because the expression IR
    /// generalises both sides of the comparison (neither operand has
    /// to be a bare column), making the Phase 2 column-vs-literal
    /// leaf path the wrong abstraction for this emission.
    Expr(crate::expr::Expr<bool>),

    /// `col @> $1` — array contains. The column must be a Postgres array type;
    /// `values` is a bound array parameter holding the elements to test for.
    ///
    /// Produced by [`crate::query::field::FieldRef::<M, Vec<V>>::contains`].
    ArrayContains(crate::array::ArrayContainsLeaf),

    /// `col <@ $1` — array contained by. Every element of `col` must also
    /// appear in the argument array.
    ///
    /// Produced by [`crate::query::field::FieldRef::<M, Vec<V>>::contained_by`].
    ArrayContainedBy(crate::array::ArrayContainedByLeaf),

    /// `col && $1` — array overlap. The column and argument share at least one
    /// element.
    ///
    /// Produced by [`crate::query::field::FieldRef::<M, Vec<V>>::overlap`].
    ArrayOverlap(crate::array::ArrayOverlapLeaf),

    /// JSONB flat-path comparison — `(col->'a'->>'b')::cast op $1`.
    ///
    /// Produced by [`crate::jsonb::path::JsonbPathRef`] comparison methods.
    /// The expression SQL is pre-rendered from validated identifiers.
    JsonbPath(crate::jsonb::path::JsonbPathLeaf),
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
    /// Whether this condition is structurally equivalent to TRUE — used by the
    /// WHERE-clause emitter (model and visage paths) to skip the clause
    /// entirely rather than emit `WHERE TRUE`. Cleaner logs, no chance of an
    /// optimizer surprise on trivially-true predicates.
    ///
    /// Recognises the shapes the queryset constructors actually produce:
    /// `True`, `And(empty | all-True)`, and `Not(Or(empty))`. Other shapes
    /// (`Or(all-True)`, `Not(And(empty))`) are not collapsed because the
    /// builder API doesn't construct them.
    pub(crate) fn is_vacuously_true(&self) -> bool {
        match self {
            Condition::True => true,
            Condition::And(xs) => xs.iter().all(Condition::is_vacuously_true),
            Condition::Not(inner) => {
                matches!(inner.as_ref(), Condition::Or(xs) if xs.is_empty())
            }
            _ => false,
        }
    }
}

impl Condition {
    /// Combine two conditions with SQL AND. Flattens nested `And` trees to
    /// keep the structure shallow for the emitter.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
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
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
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
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
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
///
/// Marked `#[non_exhaustive]` — later phases (array ops, JSONB lookups,
/// trigram search) extend this set, and downstream exhaustive matches
/// would break on every such addition. External pattern matches must
/// include a `_ => …` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
///
/// Marked `#[non_exhaustive]` — new SQL-bindable types (e.g. `Decimal`,
/// `Interval`, JSONB payload variants) are added in later phases. Adding a
/// variant must not break downstream code that pattern-matches on this
/// enum, so external matches must include a `_ => …` arm.
#[derive(Debug, Clone)]
#[non_exhaustive]
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
    /// Reverse-chronological `HeerId` — stored as BIGINT, same Postgres
    /// surface as [`FilterValue::HeerId`]. Kept as a distinct variant so
    /// `FieldRef<M, HeerIdDesc>::eq(x)` can preserve the type identity all
    /// the way into the emitted bind site (Phase 7-Zero v3).
    HeerIdDesc(crate::types::HeerIdDesc),
    /// Reverse-chronological `RanjId` — stored as UUID, same Postgres
    /// surface as [`FilterValue::RanjId`]. See [`FilterValue::HeerIdDesc`].
    RanjIdDesc(crate::types::RanjIdDesc),
    Null,
    /// For IN (...) / NOT IN (...) list lookups.
    ///
    /// # Invariants (enforced by construction, not the enum itself)
    ///
    /// - Elements must be SQL-bindable **scalars** — never nested `List` or
    ///   `Pair`. The typed `FieldRef<M, V>::in_list(impl IntoIterator<Item = V>)`
    ///   API prevents nesting by construction; manual `FilterValue::List`
    ///   construction that violates this invariant is a framework bug that
    ///   Task 6's SQL emitter panics on (not silently miscompiles).
    /// - All elements should be the same `FilterValue` discriminant (mixed-type
    ///   lists are meaningless for SQL `IN`). The typed API enforces this.
    List(Vec<FilterValue>),
    /// BETWEEN a AND b payload (two bound values).
    Pair(Box<FilterValue>, Box<FilterValue>),
    /// `NUMERIC` / `DECIMAL` column values (Phase 5).
    Decimal(rust_decimal::Decimal),

    // ── Array variants (Phase 5 Task 5) ──────────────────────────────────
    //
    // Used exclusively by the array column operators (`@>`, `<@`, `&&`).
    // Each variant binds a typed `Vec<V>` as a Postgres array parameter
    // so tokio-postgres can encode it with the correct OID. Mixed-type
    // arrays are not representable — the typed `FieldRef<M, Vec<V>>` API
    // prevents construction.
    /// `Vec<String>` array parameter (TEXT[]).
    ArrayString(Vec<String>),
    /// `Vec<i32>` array parameter (INT4[]).
    ArrayI32(Vec<i32>),
    /// `Vec<i64>` array parameter (INT8[]).
    ArrayI64(Vec<i64>),
    /// `Vec<bool>` array parameter (BOOL[]).
    ArrayBool(Vec<bool>),
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

    #[test]
    fn and_flattens_three_levels_of_nesting() {
        let a = Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true)));
        let b = Condition::Leaf(Leaf::eq_raw("b", FilterValue::Bool(false)));
        let c = Condition::Leaf(Leaf::eq_raw("c", FilterValue::Bool(true)));
        let d = Condition::Leaf(Leaf::eq_raw("d", FilterValue::Bool(false)));
        // ((a AND b) AND c) AND d → 4-leaf flat And
        let combined = Condition::and(Condition::and(Condition::and(a, b), c), d);
        if let Condition::And(parts) = combined {
            assert_eq!(parts.len(), 4, "3-deep nesting should flatten to 4 leaves");
        } else {
            panic!("expected And, got {combined:?}");
        }
    }

    #[test]
    fn and_preserves_order_with_rhs_container() {
        // (leaf_x AND (And[a, b])) should yield [leaf_x, a, b] in order
        let x = Condition::Leaf(Leaf::eq_raw("x", FilterValue::Bool(true)));
        let a = Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true)));
        let b = Condition::Leaf(Leaf::eq_raw("b", FilterValue::Bool(false)));
        let combined = Condition::and(x, Condition::And(vec![a, b]));
        if let Condition::And(parts) = combined {
            assert_eq!(parts.len(), 3);
            // Check ordering via column names stored in leaves
            let names: Vec<&'static str> = parts
                .iter()
                .filter_map(|p| {
                    if let Condition::Leaf(l) = p {
                        Some(l.column)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(names, vec!["x", "a", "b"]);
        } else {
            panic!("expected And, got {combined:?}");
        }
    }

    #[test]
    fn or_empty_vec_is_not_auto_replaced() {
        // Invariant check: Or(vec![]) stays Or(vec![]) — it is emitter's job
        // to render FALSE. Construct directly (not via `or()` — that flattens).
        let empty = Condition::Or(Vec::new());
        assert!(matches!(empty, Condition::Or(ref v) if v.is_empty()));
    }
}
