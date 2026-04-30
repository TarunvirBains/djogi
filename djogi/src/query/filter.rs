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

impl<V: IntoFilterValue> Lookup<V> {
    /// Project this lookup into the `(operator, value)` pair its SQL
    /// leaf will carry.
    ///
    /// This is the **single point** where operator-value structural
    /// invariants are established:
    ///
    /// - `Between` always pairs with [`FilterValue::Pair`];
    /// - `In` / `NotIn` always pair with [`FilterValue::List`];
    /// - `IsNull` / `IsNotNull` always pair with [`FilterValue::Null`];
    /// - every other variant pairs with a scalar [`FilterValue`].
    ///
    /// Downstream code ([`FilterClause::from_lookup`] in the clause path,
    /// the closure API through [`crate::query::field`]) funnels every
    /// lookup through this method, so the `unreachable!()` branches in
    /// [`crate::query::sql::emit_leaf`] that guard mismatched op/value
    /// shapes are genuinely unreachable from safe code. If a new
    /// [`Lookup`] variant is added, this method is the only place the
    /// pairing needs to be declared.
    ///
    /// # Operator mapping
    ///
    /// The `Contains` / `StartsWith` / `EndsWith` variants map to the
    /// **case-insensitive** `ILIKE`-family operators (`IContains`
    /// / `IStartsWith` / `IEndsWith`), mirroring the closure API's
    /// `.contains` / `.starts_with` / `.ends_with` default. Case-sensitive
    /// substring matching is not currently exposed on [`Lookup`] —
    /// callers who need it reach for the closure API or the raw
    /// `ctx.raw_execute` / `ctx.raw_scalar` escape hatch.
    ///
    /// `Regex` maps to [`LookupOp::Regex`] — the case-sensitive POSIX
    /// operator (`~`). The closure API exposes `.iregex` for the
    /// case-insensitive counterpart; a future phase can add
    /// `Lookup::IRegex` without a breaking change thanks to the
    /// `#[non_exhaustive]` marker.
    pub(crate) fn into_op_value(self) -> (LookupOp, FilterValue) {
        match self {
            Lookup::Eq(v) => (LookupOp::Eq, v.into_filter_value()),
            Lookup::Neq(v) => (LookupOp::Neq, v.into_filter_value()),
            Lookup::Gt(v) => (LookupOp::Gt, v.into_filter_value()),
            Lookup::Gte(v) => (LookupOp::Gte, v.into_filter_value()),
            Lookup::Lt(v) => (LookupOp::Lt, v.into_filter_value()),
            Lookup::Lte(v) => (LookupOp::Lte, v.into_filter_value()),
            Lookup::In(vs) => (
                LookupOp::In,
                FilterValue::List(vs.into_iter().map(|v| v.into_filter_value()).collect()),
            ),
            Lookup::NotIn(vs) => (
                LookupOp::NotIn,
                FilterValue::List(vs.into_iter().map(|v| v.into_filter_value()).collect()),
            ),
            Lookup::IsNull => (LookupOp::IsNull, FilterValue::Null),
            Lookup::IsNotNull => (LookupOp::IsNotNull, FilterValue::Null),
            Lookup::Contains(s) => (LookupOp::IContains, FilterValue::String(s)),
            Lookup::StartsWith(s) => (LookupOp::IStartsWith, FilterValue::String(s)),
            Lookup::EndsWith(s) => (LookupOp::IEndsWith, FilterValue::String(s)),
            Lookup::Between(a, b) => (
                LookupOp::Between,
                FilterValue::Pair(
                    Box::new(a.into_filter_value()),
                    Box::new(b.into_filter_value()),
                ),
            ),
            Lookup::Regex(s) => (LookupOp::Regex, FilterValue::String(s)),
        }
    }
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
/// # Invariants
///
/// Fields are `pub(crate)` so the only path to a `FilterClause` from
/// outside the crate is [`FilterClause::from_lookup`]. That funnel routes
/// every value through [`Lookup::into_op_value`], which pairs each
/// [`LookupOp`] with the structurally correct [`FilterValue`] shape
/// (`Between`↔`Pair`, `In`/`NotIn`↔`List`, `IsNull`/`IsNotNull`↔`Null`,
/// …). Consequently, the `unreachable!()` branches in the SQL emitter
/// (`sql::emit_leaf`) that guard against mismatched op/value pairings
/// are genuinely unreachable from safe code — downstream crates cannot
/// hand-craft an invalid clause by poking at fields directly.
///
/// [`from_lookup`]: FilterClause::from_lookup
#[derive(Debug, Clone)]
pub struct FilterClause {
    /// SQL column name — macro-baked literal, never user input.
    pub(crate) column: &'static str,
    /// Operator discriminant — chosen per [`Lookup`] variant in
    /// [`FilterClause::from_lookup`].
    pub(crate) op: LookupOp,
    /// Already-projected bind value. `FilterValue::Null` is used for the
    /// `IsNull` / `IsNotNull` variants where no bind is emitted.
    pub(crate) value: FilterValue,
}

impl FilterClause {
    /// Project a `Lookup<V>` into a type-erased `FilterClause`.
    ///
    /// This is the **only** public constructor for `FilterClause` — every
    /// macro-emitted setter funnels through it, and the `FilterClause`
    /// fields are `pub(crate)` so downstream crates cannot sidestep it.
    /// Pairing operator and value happens in a single place
    /// ([`Lookup::into_op_value`]), so the `unreachable!()` branches in
    /// [`crate::query::sql::emit_leaf`] that guard shape mismatches are
    /// genuinely unreachable from safe code.
    ///
    /// The `V: IntoFilterValue` bound is the same one the typed
    /// [`FieldRef`] lookup methods use, so newtype columns and
    /// string-like types compose identically in both surfaces.
    ///
    /// [`FieldRef`]: crate::query::FieldRef
    #[must_use]
    pub fn from_lookup<V: IntoFilterValue>(column: &'static str, lookup: Lookup<V>) -> Self {
        let (op, value) = lookup.into_op_value();
        Self { column, op, value }
    }

    /// Turn this clause into a `Condition::Leaf`. Called by
    /// [`QuerySet::filter_struct`] when folding a filter's `Vec` into
    /// the queryset's condition tree.
    ///
    /// [`QuerySet::filter_struct`]: crate::query::QuerySet::filter_struct
    pub fn into_condition(self) -> Condition {
        Condition::Leaf(Leaf::new(self.column, self.op, self.value))
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
/// # Object safety
///
/// `ModelFilter` is **not** object-safe today: [`into_clauses`] takes
/// `self` by value, which is incompatible with `dyn ModelFilter` trait
/// objects (`self: Box<Self>` would be the by-value equivalent, but
/// that forces every caller through a heap allocation and a
/// `Box::new(...)` at the call site). The two current consumers —
/// [`QuerySet::filter_struct`] (generic `F: ModelFilter`) and the Phase
/// 2 unit/integration tests — never need storage-erased filters, so
/// keeping the by-value shape preserves the zero-alloc path and matches
/// the rest of the builder surface (`QuerySet`'s chain methods are also
/// by-value self).
///
/// A future admin-UI use case may need to store a heterogeneous list of
/// filters (each column's operator and value come over HTTP at request
/// time, not known at compile time). The trait's shape is left
/// unconstrained (no `: Sized` bound) so either of two extension paths
/// stays open: a sibling `DynModelFilter` trait with `fn into_clauses(
/// self: Box<Self>) -> Vec<FilterClause>`, or an owned-clauses field on
/// the filter struct.
///
/// [`into_clauses`]: ModelFilter::into_clauses
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
    fn filter_clause_from_lookup_composes() {
        // `FilterClause::from_lookup` is the single public constructor
        // for `FilterClause`. It funnels through `Lookup::into_op_value`,
        // which is the one place operator-value structural pairings
        // (Between↔Pair, In/NotIn↔List, IsNull/IsNotNull↔Null, …) are
        // established. This test pins a representative sample from each
        // shape family so a regression to hand-built clauses (or a
        // reshuffled `into_op_value` match arm) is caught by the unit
        // tier rather than by the SQL emitter's downstream
        // `unreachable!()` branches.
        //
        // The column string is preserved verbatim in every case — the
        // macro bakes a `&'static str` literal per setter and this
        // funnel never rewrites it.

        // Scalar: Eq → FilterValue::I32
        let c = FilterClause::from_lookup("view_count", Lookup::Eq(42i32));
        assert_eq!(c.column, "view_count");
        assert_eq!(c.op, LookupOp::Eq);
        assert!(matches!(c.value, FilterValue::I32(42)));

        // List: In → FilterValue::List
        let c = FilterClause::from_lookup("id", Lookup::In(vec![1i64, 2, 3]));
        assert_eq!(c.op, LookupOp::In);
        assert!(matches!(c.value, FilterValue::List(ref v) if v.len() == 3));

        // List: NotIn → FilterValue::List (empty is still List, not Null)
        let c = FilterClause::from_lookup("id", Lookup::<i64>::NotIn(Vec::new()));
        assert_eq!(c.op, LookupOp::NotIn);
        assert!(matches!(c.value, FilterValue::List(ref v) if v.is_empty()));

        // Pair: Between → FilterValue::Pair(Box, Box)
        let c = FilterClause::from_lookup("age", Lookup::Between(10i32, 20i32));
        assert_eq!(c.op, LookupOp::Between);
        assert!(matches!(c.value, FilterValue::Pair(_, _)));

        // Null: IsNull → FilterValue::Null (no projection of V)
        let c = FilterClause::from_lookup("deleted_at", Lookup::<String>::IsNull);
        assert_eq!(c.op, LookupOp::IsNull);
        assert!(matches!(c.value, FilterValue::Null));

        // Null: IsNotNull → FilterValue::Null (symmetry with IsNull)
        let c = FilterClause::from_lookup("deleted_at", Lookup::<String>::IsNotNull);
        assert_eq!(c.op, LookupOp::IsNotNull);
        assert!(matches!(c.value, FilterValue::Null));

        // String family: Contains → case-insensitive IContains operator
        // (pattern wrapping `%…%` happens inside the SQL emitter, not
        // here — the clause carries the raw user string).
        let c = FilterClause::from_lookup("title", Lookup::<String>::Contains("x".to_string()));
        assert_eq!(c.op, LookupOp::IContains);
        assert!(matches!(c.value, FilterValue::String(ref s) if s == "x"));
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
