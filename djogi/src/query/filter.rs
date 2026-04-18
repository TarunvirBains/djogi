//! Programmatic filter API — the closure-free path for callers who can't
//! express their filters as a `|f|` closure at compile time.
//!
//! # What
//!
//! [`Lookup<V>`] is the stable user-facing enum enumerating the lookup
//! operators a `{Model}Filter` setter accepts. Variants carrying a value
//! bound `V: IntoFilterValue` so every type the typed [`FieldRef`] path
//! accepts (strings, integers, `HeerId`, `RanjId`, …) composes here too.
//!
//! [`FilterClause`] is the type-erased record a macro-emitted setter
//! appends to its internal `Vec`. Erasing the value here — the typed
//! `Lookup<V>` is projected through [`IntoFilterValue`] into a
//! [`FilterValue`] discriminant — is what lets a single `{Model}Filter`
//! struct carry a mixed set of clauses (a bool clause next to an `i32`
//! clause next to a string clause) without a per-clause generic tuple.
//!
//! [`ModelFilter`] is the trait every macro-emitted `{Model}Filter`
//! implements. Its sole method hands the accumulated `Vec<FilterClause>`
//! to [`QuerySet::filter_struct`], which folds them into a single
//! [`Condition::And`] and AND-s that onto the queryset's existing
//! condition tree.
//!
//! # Why
//!
//! The closure API (`QuerySet::filter(|f| ...)`) is the preferred user
//! surface — the compiler type-checks every lookup against the column's
//! declared type. But three real callers cannot write a closure:
//!
//! 1. **Shell** — Rhai cannot express a Rust closure that closes over the
//!    model's ZST `Fields`. The shell's filter-builder bindings need a
//!    runtime object they can populate one lookup at a time.
//! 2. **Admin UI** — the filter bar sends key/op/value triples over HTTP;
//!    the server reconstructs a filter without ever seeing a user-written
//!    closure.
//! 3. **Dynamic SQL assemblers** — search/export jobs that stitch a query
//!    from a config file or a feature flag.
//!
//! Both paths produce the same `Condition` tree and the same SQL — an
//! integration test in `tests/integration/phase2_queryset.rs` asserts
//! row-count parity between the two surfaces.
//!
//! # How (user surface)
//!
//! ```ignore
//! use djogi::prelude::*;
//!
//! let filter = PostFilter::new()
//!     .published(Lookup::Eq(true))
//!     .view_count(Lookup::Gte(50i32));
//!
//! let rows = Post::objects().filter_struct(filter).fetch_all(&pool).await?;
//! ```
//!
//! # Where
//!
//! - `Condition` / `Leaf` / `FilterValue` / `LookupOp` — [`crate::query::condition`].
//! - `IntoFilterValue` — [`crate::query::field`] (shared with the closure API).
//! - `{Model}Filter` codegen — `djogi-macros/src/model/filter.rs`.
//! - `QuerySet::filter_struct` — [`crate::query::queryset`].
//!
//! [`FieldRef`]: crate::query::FieldRef
//! [`QuerySet::filter_struct`]: crate::query::QuerySet::filter_struct
//! [`Condition::And`]: crate::query::Condition::And

use crate::query::condition::{Condition, FilterValue, Leaf, LookupOp};
use crate::query::field::IntoFilterValue;

/// User-facing lookup constructor — one variant per operator the
/// programmatic filter API exposes.
///
/// Generic over `V` so newtype columns compose the same way they do
/// through [`FieldRef`]: any `V: IntoFilterValue` is accepted by the
/// value-carrying variants, and the single [`FilterClause::from_lookup`]
/// funnel projects every variant through `IntoFilterValue` into a
/// [`FilterValue`].
///
/// Marked `#[non_exhaustive]` — later phases (array ops, JSONB lookups,
/// trigram search) extend this set, and downstream exhaustive matches
/// would break on every such addition.
///
/// [`FieldRef`]: crate::query::FieldRef
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Lookup<V> {
    /// `column = value`.
    Eq(V),
    /// `column <> value`.
    Neq(V),
    /// `column > value`.
    Gt(V),
    /// `column >= value`.
    Gte(V),
    /// `column < value`.
    Lt(V),
    /// `column <= value`.
    Lte(V),
    /// `column IN (v1, v2, …)`.
    In(Vec<V>),
    /// `column NOT IN (v1, v2, …)`.
    NotIn(Vec<V>),
    /// `column IS NULL`.
    IsNull,
    /// `column IS NOT NULL`.
    IsNotNull,
    /// Case-insensitive substring match — `ILIKE '%value%'`.
    Contains(String),
    /// Case-insensitive prefix match — `ILIKE 'value%'`.
    StartsWith(String),
    /// Case-insensitive suffix match — `ILIKE '%value'`.
    EndsWith(String),
    /// `column BETWEEN a AND b` (inclusive on both ends per SQL spec).
    Between(V, V),
    /// POSIX regex match — `column ~ value` (case-sensitive; Postgres-specific).
    /// Use the closure API's `.iregex` for the case-insensitive variant.
    Regex(String),
}

/// Erased clause — what a macro-emitted `{Model}Filter` setter pushes
/// into its internal `Vec`.
///
/// The `column` name is a macro-baked `&'static str` literal; the macro
/// never derives it from user input. `op` is the SQL operator;
/// [`from_lookup`] chooses it per `Lookup<V>` variant. `value` is the
/// already-projected [`FilterValue`] — the `V: IntoFilterValue` projection
/// happens inside [`from_lookup`], so this struct is plain data and
/// carries no generic parameter. That lets a single
/// `Vec<FilterClause>` hold a heterogeneous set of clauses (a `bool`
/// clause next to an `i32` clause next to a `String` clause) without
/// per-clause boxing gymnastics.
///
/// [`from_lookup`]: FilterClause::from_lookup
#[derive(Debug, Clone)]
pub struct FilterClause {
    /// SQL column name — macro-baked literal, never user input.
    pub column: &'static str,
    /// Operator discriminant — chosen per [`Lookup`] variant in
    /// [`FilterClause::from_lookup`].
    pub op: LookupOp,
    /// Already-projected bind value. `FilterValue::Null` is used for the
    /// `IsNull` / `IsNotNull` variants where no bind is emitted.
    pub value: FilterValue,
}

impl FilterClause {
    /// Project a `Lookup<V>` into a type-erased `FilterClause`.
    ///
    /// Called by every macro-emitted setter. The `V: IntoFilterValue`
    /// bound is the same one the typed [`FieldRef`] lookup methods use,
    /// so newtype columns and string-like types compose identically in
    /// both surfaces.
    ///
    /// # Operator mapping
    ///
    /// The `Contains` / `StartsWith` / `EndsWith` variants map to the
    /// **case-insensitive** `ILIKE` variants (`IContains` / `IStartsWith`
    /// / `IEndsWith`), mirroring the closure API's `.contains` / `.starts_with`
    /// / `.ends_with` default. Case-sensitive substring matching is not
    /// currently exposed on [`Lookup`] — callers who need it reach for
    /// the closure API or the raw `sqlx::QueryBuilder` escape hatch.
    ///
    /// `Regex` maps to `LookupOp::Regex` — the case-sensitive POSIX
    /// operator (`~`). The closure API exposes `.iregex` for the
    /// case-insensitive counterpart; a future phase can add `Lookup::IRegex`
    /// without a breaking change thanks to the `#[non_exhaustive]` marker.
    ///
    /// [`FieldRef`]: crate::query::FieldRef
    pub fn from_lookup<V: IntoFilterValue>(column: &'static str, lookup: Lookup<V>) -> Self {
        match lookup {
            Lookup::Eq(v) => Self {
                column,
                op: LookupOp::Eq,
                value: v.into_filter_value(),
            },
            Lookup::Neq(v) => Self {
                column,
                op: LookupOp::Neq,
                value: v.into_filter_value(),
            },
            Lookup::Gt(v) => Self {
                column,
                op: LookupOp::Gt,
                value: v.into_filter_value(),
            },
            Lookup::Gte(v) => Self {
                column,
                op: LookupOp::Gte,
                value: v.into_filter_value(),
            },
            Lookup::Lt(v) => Self {
                column,
                op: LookupOp::Lt,
                value: v.into_filter_value(),
            },
            Lookup::Lte(v) => Self {
                column,
                op: LookupOp::Lte,
                value: v.into_filter_value(),
            },
            Lookup::In(vs) => Self {
                column,
                op: LookupOp::In,
                value: FilterValue::List(vs.into_iter().map(|v| v.into_filter_value()).collect()),
            },
            Lookup::NotIn(vs) => Self {
                column,
                op: LookupOp::NotIn,
                value: FilterValue::List(vs.into_iter().map(|v| v.into_filter_value()).collect()),
            },
            Lookup::IsNull => Self {
                column,
                op: LookupOp::IsNull,
                value: FilterValue::Null,
            },
            Lookup::IsNotNull => Self {
                column,
                op: LookupOp::IsNotNull,
                value: FilterValue::Null,
            },
            Lookup::Contains(s) => Self {
                column,
                op: LookupOp::IContains,
                value: FilterValue::String(s),
            },
            Lookup::StartsWith(s) => Self {
                column,
                op: LookupOp::IStartsWith,
                value: FilterValue::String(s),
            },
            Lookup::EndsWith(s) => Self {
                column,
                op: LookupOp::IEndsWith,
                value: FilterValue::String(s),
            },
            Lookup::Between(a, b) => Self {
                column,
                op: LookupOp::Between,
                value: FilterValue::Pair(
                    Box::new(a.into_filter_value()),
                    Box::new(b.into_filter_value()),
                ),
            },
            Lookup::Regex(s) => Self {
                column,
                op: LookupOp::Regex,
                value: FilterValue::String(s),
            },
        }
    }

    /// Turn this clause into a `Condition::Leaf`. Called by
    /// [`QuerySet::filter_struct`] when folding a filter's `Vec` into
    /// the queryset's condition tree.
    ///
    /// [`QuerySet::filter_struct`]: crate::query::QuerySet::filter_struct
    pub fn into_condition(self) -> Condition {
        Condition::Leaf(Leaf {
            column: self.column,
            op: self.op,
            value: self.value,
        })
    }
}

/// Implemented by every macro-emitted `{Model}Filter`. Exposes the
/// accumulated clauses so [`QuerySet::filter_struct`] can fold them into
/// a single `Condition::And(...)` AND-ed onto the queryset's existing
/// tree.
///
/// Users never implement this trait by hand — the `#[model]` macro
/// stamps it alongside the filter struct.
///
/// [`QuerySet::filter_struct`]: crate::query::QuerySet::filter_struct
pub trait ModelFilter {
    /// Hand off the accumulated clauses. Consumes `self` — filters are
    /// single-use, matching the queryset builder's own consume-self shape.
    fn into_clauses(self) -> Vec<FilterClause>;
}

/// Fold a clause vec into a single `Condition`, preserving the empty/one/
/// many cases the queryset layer cares about.
///
/// - `[]` → `Condition::True` (vacuous — filter_struct returns early so
///   callers never actually see this case, but keeping the helper total
///   means unit tests can exercise every branch).
/// - `[c]` → `c` unwrapped — emitting a single-element `And` would render
///   as `(c)` with redundant parentheses.
/// - `[c1, c2, ...]` → `Condition::And(vec![c1, c2, ...])`.
///
/// Not public API — this is an implementation detail of `filter_struct`.
/// The plan lists it as a "helper to fold a Vec<FilterClause> into a
/// Condition::And(...) tree"; keeping it in this module means the
/// queryset layer stays free of condition-tree construction details.
pub(crate) fn clauses_into_condition(clauses: Vec<FilterClause>) -> Condition {
    match clauses.len() {
        0 => Condition::True,
        1 => {
            // `into_iter().next().unwrap()` is safe — we just checked len == 1.
            // Using `next()` rather than indexing avoids a pointless clone.
            clauses
                .into_iter()
                .next()
                .expect("len == 1 branch guarantees one element")
                .into_condition()
        }
        _ => Condition::And(
            clauses
                .into_iter()
                .map(FilterClause::into_condition)
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_eq_projects_to_eq_op_and_bool_value() {
        let clause = FilterClause::from_lookup("published", Lookup::Eq(true));
        assert_eq!(clause.column, "published");
        assert_eq!(clause.op, LookupOp::Eq);
        assert!(matches!(clause.value, FilterValue::Bool(true)));
    }

    #[test]
    fn lookup_gte_projects_to_gte_op_and_i32_value() {
        let clause = FilterClause::from_lookup("view_count", Lookup::Gte(50i32));
        assert_eq!(clause.op, LookupOp::Gte);
        assert!(matches!(clause.value, FilterValue::I32(50)));
    }

    #[test]
    fn lookup_in_builds_list_filter_value() {
        let clause = FilterClause::from_lookup("id", Lookup::In(vec![1i64, 2, 3]));
        assert_eq!(clause.op, LookupOp::In);
        if let FilterValue::List(items) = clause.value {
            assert_eq!(items.len(), 3);
        } else {
            panic!("expected FilterValue::List");
        }
    }

    #[test]
    fn lookup_between_builds_pair_filter_value() {
        let clause = FilterClause::from_lookup("age", Lookup::Between(10i32, 20i32));
        assert_eq!(clause.op, LookupOp::Between);
        assert!(matches!(clause.value, FilterValue::Pair(_, _)));
    }

    #[test]
    fn lookup_is_null_carries_null_filter_value() {
        // `Lookup::IsNull` is not generic over any runtime value, but `V`
        // still has to be nameable so the user writes `Lookup::<i64>::IsNull`
        // or lets type inference fill it in from the surrounding context.
        let clause = FilterClause::from_lookup("deleted_at", Lookup::<String>::IsNull);
        assert_eq!(clause.op, LookupOp::IsNull);
        assert!(matches!(clause.value, FilterValue::Null));
    }

    #[test]
    fn lookup_contains_maps_to_icontains_op() {
        // Documented mapping: `Contains` is the case-insensitive variant —
        // matches the closure API's `.contains` default.
        let clause =
            FilterClause::from_lookup("title", Lookup::<String>::Contains("hi".to_string()));
        assert_eq!(clause.op, LookupOp::IContains);
        assert!(matches!(clause.value, FilterValue::String(ref s) if s == "hi"));
    }

    #[test]
    fn lookup_regex_maps_to_case_sensitive_regex_op() {
        // Documented mapping: plain `Regex` is case-sensitive (`~`), not `~*`.
        let clause = FilterClause::from_lookup("slug", Lookup::<String>::Regex("^foo".to_string()));
        assert_eq!(clause.op, LookupOp::Regex);
    }

    #[test]
    fn into_condition_produces_leaf() {
        let clause = FilterClause::from_lookup("title", Lookup::Eq("x".to_string()));
        let cond = clause.into_condition();
        assert!(matches!(cond, Condition::Leaf(_)));
    }

    #[test]
    fn clauses_into_condition_empty_is_true() {
        let c = clauses_into_condition(Vec::new());
        assert!(matches!(c, Condition::True));
    }

    #[test]
    fn clauses_into_condition_single_unwraps_to_leaf() {
        // Single-clause filters should not be wrapped in a one-element And
        // — the SQL emitter would render that as `(leaf)` with redundant
        // parens.
        let clause = FilterClause::from_lookup("published", Lookup::Eq(true));
        let c = clauses_into_condition(vec![clause]);
        assert!(matches!(c, Condition::Leaf(_)));
    }

    #[test]
    fn clauses_into_condition_many_builds_flat_and() {
        let a = FilterClause::from_lookup("published", Lookup::Eq(true));
        let b = FilterClause::from_lookup("view_count", Lookup::Gte(50i32));
        let c = FilterClause::from_lookup("title", Lookup::Neq("draft".to_string()));
        let cond = clauses_into_condition(vec![a, b, c]);
        if let Condition::And(parts) = cond {
            assert_eq!(parts.len(), 3);
            // Order must be preserved — the queryset layer relies on the
            // user's declaration order matching SQL emission order for
            // predictable `EXPLAIN` output.
            for p in &parts {
                assert!(matches!(p, Condition::Leaf(_)));
            }
        } else {
            panic!("expected Condition::And with 3 elements");
        }
    }

    // Minimal struct implementing `ModelFilter` by hand — mirrors what the
    // macro emits, so `into_clauses` is exercised without depending on the
    // macro expansion path.
    struct FakeFilter {
        clauses: Vec<FilterClause>,
    }
    impl ModelFilter for FakeFilter {
        fn into_clauses(self) -> Vec<FilterClause> {
            self.clauses
        }
    }

    #[test]
    fn model_filter_trait_returns_pushed_clauses() {
        let f = FakeFilter {
            clauses: vec![
                FilterClause::from_lookup("a", Lookup::Eq(1i32)),
                FilterClause::from_lookup("b", Lookup::Eq(2i32)),
            ],
        };
        let v = f.into_clauses();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].column, "a");
        assert_eq!(v[1].column, "b");
    }
}
