//! Typed column handles — the API surface every filter closure interacts with.
//!
//! # What
//!
//! `FieldRef<M, V>` is the handle returned by macro-generated `{Model}Fields`
//! methods (Task 4). It carries only a `'static` column name plus phantom
//! type parameters that tie it to a specific `Model` (`M`) and a specific
//! SQL-bindable value type (`V`). Each method call (`.eq`, `.gte`, etc.)
//! consumes the ref (by value — it's `Copy`) and a value, returning a
//! `Condition::Leaf` that slots into the `QuerySet<T>` filter tree.
//!
//! # Why
//!
//! - **Type safety.** `M` binds a ref to one model, so mixing
//!   `UserFields.id()` with a `Post`-targeted QuerySet is a compile error,
//!   not a runtime SQL error. `V` binds the lookup's RHS to the column's
//!   Rust type, so `view_count.eq("hello")` fails at `IntoFilterValue` —
//!   again at compile time.
//! - **Zero runtime cost.** `FieldRef` is two phantom markers plus a
//!   `&'static str`; the whole struct is `Copy` and disappears after the
//!   leaf is built. No boxing, no reflection, no string formatting.
//! - **String-only lookups are gated.** Methods like `contains`/
//!   `starts_with`/`regex` live in an `impl<M: Model> FieldRef<M, String>`
//!   block so calling `age.contains(...)` yields a helpful "no method named
//!   `contains` on `FieldRef<M, i64>`" error — the type system is the
//!   documentation.
//!
//! # How (user surface)
//!
//! ```ignore
//! use djogi::prelude::*;
//!
//! let qs = Post::objects().filter(|f| {
//!     f.published().eq(true)
//!         .and_with(f.view_count().gte(100))
//! });
//! ```
//!
//! Users never call `FieldRef::new` directly — the macro stamps it for each
//! column. The `#[doc(hidden)]` constructor exists so macro output compiles,
//! not for hand-written code.
//!
//! # Where
//!
//! - `Condition` / `Leaf` / `FilterValue` / `LookupOp` — `query::condition`.
//! - `{Model}Fields` generation — `djogi-macros/src/model/fields.rs` (Task 4).
//! - `Model::Fields` associated type — `djogi/src/model.rs`.

use crate::model::Model;
use crate::query::condition::{Condition, FilterValue, Leaf, LookupOp};
use std::marker::PhantomData;

/// Typed reference to a model column.
///
/// Produced by macro-generated `{Model}Fields` methods and consumed by
/// lookup methods (`eq`, `gte`, `contains`, …) to build `Condition::Leaf`
/// nodes. `Copy + 'static` because closures move it freely and the column
/// name is always a literal baked into the macro output.
///
/// The `PhantomData<fn() -> M>` / `PhantomData<fn() -> V>` markers ensure
/// `FieldRef` is `Send + Sync` even when `M` or `V` are not — the ref never
/// owns or borrows a value of either type, it merely tags the column.
pub struct FieldRef<M: Model, V> {
    column: &'static str,
    _m: PhantomData<fn() -> M>,
    _v: PhantomData<fn() -> V>,
}

impl<M: Model, V> Copy for FieldRef<M, V> {}
impl<M: Model, V> Clone for FieldRef<M, V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M: Model, V> std::fmt::Debug for FieldRef<M, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FieldRef({})", self.column)
    }
}

impl<M: Model, V> FieldRef<M, V> {
    /// Construct a new `FieldRef`. Crate-private so downstream code
    /// cannot fabricate a ref whose `column` string smuggles SQL into
    /// the `sqlx::QueryBuilder::push` sites in `query::sql`. The macro
    /// reaches this constructor through
    /// [`__macro_support::__make_field_ref`], which validates the
    /// column name against [`crate::ident::assert_plain_ident`]
    /// before instantiation. `const` so the macro-emitted
    /// `{Model}Fields` accessors stay trivially inlinable.
    pub(crate) const fn new(column: &'static str) -> Self {
        Self {
            column,
            _m: PhantomData,
            _v: PhantomData,
        }
    }

    /// Internal accessor for the column name. Used by the SQL emitter
    /// (`query::sql`, Task 6) and by `QuerySet::distinct_on` (Task 5).
    #[doc(hidden)]
    pub fn column(self) -> &'static str {
        self.column
    }
}

/// Macro-only entry points. **Not** part of the stable public API.
///
/// `djogi-macros` emits calls into this module from user-crate code
/// that `#[derive(Model)]` expands — the items here are `pub` only so
/// cross-crate codegen can reach them. The double-underscore prefix
/// and `#[doc(hidden)]` marker signal to tooling and reviewers that
/// downstream code must not call these directly; the macro is the
/// sole supported caller.
///
/// The seal closes the same identifier-smuggling vector that
/// [`crate::relation::__macro_support`] closes for `RelationPath`:
/// `FieldRef::new` was `pub` before this seal, which let a hostile
/// downstream crate fabricate a `FieldRef` whose column string
/// carried SQL metacharacters, and those strings flowed straight
/// into `sqlx::QueryBuilder::push` inside `query::sql`'s
/// `emit_leaf`, `DISTINCT ON`, `ORDER BY`, and `UPDATE … SET`
/// emitters. Constructing a `FieldRef` now requires going through
/// [`__make_field_ref`], which routes the column name through the
/// shared [`crate::ident::assert_plain_ident`] validator.
#[doc(hidden)]
pub mod __macro_support {
    use super::FieldRef;
    use crate::ident::assert_plain_ident;
    use crate::model::Model;

    /// Construct a [`FieldRef<M, V>`] from a macro-emitted column
    /// name. The only supported caller is the `{Model}Fields::field()`
    /// accessor that `#[derive(Model)]` emits in the user's crate.
    ///
    /// Panics if `column` violates any rule in
    /// [`crate::ident::assert_plain_ident`]: empty, over 63 bytes,
    /// leading digit, a non-identifier byte, or a reserved Postgres
    /// keyword. The check is the runtime half of the seal; the
    /// compile-time half is [`FieldRef::new`] being `pub(crate)`.
    #[doc(hidden)]
    pub fn __make_field_ref<M: Model, V>(column: &'static str) -> FieldRef<M, V> {
        assert_plain_ident(column, "field_column");
        FieldRef::new(column)
    }

    #[cfg(test)]
    #[allow(clippy::manual_async_fn)]
    // The `Model` trait's CRUD methods return `impl Future + Send` rather
    // than using `async fn` syntax (pinned to Send explicitly). The inert
    // test stub below mirrors that trait shape, which trips
    // `clippy::manual_async_fn` under Rust 1.93+. Allow the lint on this
    // module only — rewriting the trait itself is out of scope for the
    // FieldRef seal.
    mod tests {
        use super::*;
        use crate::DjogiError;
        use crate::descriptor::ModelDescriptor;
        use std::future::Future;

        // Minimal inert `Model` stub. Exhaustive validator coverage
        // lives in `crate::ident::tests`; this file only verifies that
        // the `__make_field_ref` wrapper threads its column arg
        // through the shared validator before constructing the ref.
        struct M;

        impl crate::model::__sealed::Sealed for M {}
        impl Model for M {
            type Pk = crate::types::HeerId;
            type Fields = ();
            fn table_name() -> &'static str {
                "ms"
            }
            fn pk_value(&self) -> &Self::Pk {
                unreachable!()
            }
            fn descriptor() -> &'static ModelDescriptor {
                unreachable!()
            }
            fn get<'a>(
                _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
                _id: Self::Pk,
            ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
                async { unreachable!() }
            }
            fn create<'a>(
                _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
                _v: Self,
            ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
                async { unreachable!() }
            }
            fn save<'a>(
                &self,
                _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
            ) -> impl Future<Output = Result<(), DjogiError>> + Send {
                async { unreachable!() }
            }
            fn delete<'a>(
                self,
                _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
            ) -> impl Future<Output = Result<(), DjogiError>> + Send {
                async { unreachable!() }
            }
            fn refresh_from_db<'a>(
                &self,
                _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
            ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
                async { unreachable!() }
            }
        }

        fn try_make(column: &'static str) -> std::thread::Result<FieldRef<M, String>> {
            std::panic::catch_unwind(|| __make_field_ref::<M, String>(column))
        }

        #[test]
        fn accepts_plain_column_name() {
            assert!(try_make("title").is_ok());
            assert!(try_make("view_count").is_ok());
        }

        #[test]
        fn rejects_leading_digit() {
            // Would emit `SELECT 123 FROM ...` or `ORDER BY 123`
            // if it slipped through.
            assert!(try_make("1col").is_err());
        }

        #[test]
        fn rejects_reserved_keyword() {
            // Would emit `WHERE select = $1` which is a parse error.
            assert!(try_make("select").is_err());
        }

        #[test]
        fn rejects_sql_metacharacter_payload() {
            // The same shape that motivated the seal on RelationPath.
            assert!(try_make("col) OR 1=1 --").is_err());
        }
    }
}

/// Type-level bridge from user value types to `FilterValue`.
///
/// Implementing this trait on `V` enables `FieldRef<M, V>::eq(v)` /
/// `.gte(v)` / etc. No Serde, no reflection — each bindable SQL type
/// gets one impl and maps to exactly one `FilterValue` variant.
///
/// External crates cannot add new `FilterValue` variants, so extending
/// this trait for user-defined types means wrapping them in one of the
/// shipped variants (typically `String` for JSONB-friendly types).
pub trait IntoFilterValue {
    fn into_filter_value(self) -> FilterValue;
}

// One impl per SQL-bindable type Djogi ships with. New types (e.g. Decimal
// in Phase 5) extend both `FilterValue` and this trait in lockstep.
impl IntoFilterValue for String {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::String(self)
    }
}
impl IntoFilterValue for &str {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::String(self.to_owned())
    }
}
impl IntoFilterValue for i16 {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::I16(self)
    }
}
impl IntoFilterValue for i32 {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::I32(self)
    }
}
impl IntoFilterValue for i64 {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::I64(self)
    }
}
impl IntoFilterValue for f32 {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::F32(self)
    }
}
impl IntoFilterValue for f64 {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::F64(self)
    }
}
impl IntoFilterValue for bool {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::Bool(self)
    }
}
impl IntoFilterValue for time::OffsetDateTime {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::DateTime(self)
    }
}
impl IntoFilterValue for time::Date {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::Date(self)
    }
}
impl IntoFilterValue for uuid::Uuid {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::Uuid(self)
    }
}
impl IntoFilterValue for crate::HeerId {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::HeerId(self)
    }
}
impl IntoFilterValue for crate::RanjId {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::RanjId(self)
    }
}

// ── Generic lookup methods (any V: IntoFilterValue) ───────────────────────

impl<M: Model, V: IntoFilterValue> FieldRef<M, V> {
    /// `column = value` — SQL equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn eq(self, value: V) -> Condition {
        Condition::Leaf(Leaf {
            column: self.column,
            op: LookupOp::Eq,
            value: value.into_filter_value(),
        })
    }

    /// `column <> value` — SQL inequality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn neq(self, value: V) -> Condition {
        Condition::Leaf(Leaf {
            column: self.column,
            op: LookupOp::Neq,
            value: value.into_filter_value(),
        })
    }

    /// `column > value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn gt(self, value: V) -> Condition {
        Condition::Leaf(Leaf {
            column: self.column,
            op: LookupOp::Gt,
            value: value.into_filter_value(),
        })
    }

    /// `column >= value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn gte(self, value: V) -> Condition {
        Condition::Leaf(Leaf {
            column: self.column,
            op: LookupOp::Gte,
            value: value.into_filter_value(),
        })
    }

    /// `column < value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn lt(self, value: V) -> Condition {
        Condition::Leaf(Leaf {
            column: self.column,
            op: LookupOp::Lt,
            value: value.into_filter_value(),
        })
    }

    /// `column <= value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn lte(self, value: V) -> Condition {
        Condition::Leaf(Leaf {
            column: self.column,
            op: LookupOp::Lte,
            value: value.into_filter_value(),
        })
    }

    /// `column BETWEEN a AND b` (inclusive on both ends per SQL spec).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn between(self, a: V, b: V) -> Condition {
        Condition::Leaf(Leaf {
            column: self.column,
            op: LookupOp::Between,
            value: FilterValue::Pair(
                Box::new(a.into_filter_value()),
                Box::new(b.into_filter_value()),
            ),
        })
    }

    /// Case-insensitive equality — `LOWER(column) = LOWER(value)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn iexact(self, value: V) -> Condition {
        Condition::Leaf(Leaf {
            column: self.column,
            op: LookupOp::IExact,
            value: value.into_filter_value(),
        })
    }
}

// `in_list` / `not_in_list` accept any `IntoIterator<Item = V>` rather than a
// preallocated `Vec<V>`. Split into its own impl block so the generic bound
// on the list payload is visible at the call site; no functional difference
// versus folding them into the block above.
//
// Accepting `IntoIterator` matters at scale — callers building 10k+-element
// `IN (...)` filters (batch imports, bulk soft-deletes) can pipe directly
// from a `Range`, a `Map`, or a DB cursor without a preallocated Vec. `Vec<V>`
// itself still works because `Vec: IntoIterator`, so existing callsites keep
// compiling without change.
impl<M: Model, V: IntoFilterValue> FieldRef<M, V> {
    /// `column IN (v1, v2, …)`. An empty iterator is allowed and renders as
    /// SQL `FALSE` at emission time (Task 6).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn in_list<I: IntoIterator<Item = V>>(self, values: I) -> Condition {
        let list = FilterValue::List(
            values
                .into_iter()
                .map(IntoFilterValue::into_filter_value)
                .collect::<Vec<_>>(),
        );
        Condition::Leaf(Leaf {
            column: self.column,
            op: LookupOp::In,
            value: list,
        })
    }

    /// `column NOT IN (v1, v2, …)`. An empty iterator is allowed and renders
    /// as SQL `TRUE` at emission time.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_in_list<I: IntoIterator<Item = V>>(self, values: I) -> Condition {
        let list = FilterValue::List(
            values
                .into_iter()
                .map(IntoFilterValue::into_filter_value)
                .collect::<Vec<_>>(),
        );
        Condition::Leaf(Leaf {
            column: self.column,
            op: LookupOp::NotIn,
            value: list,
        })
    }
}

// ── String-only lookups ───────────────────────────────────────────────────
//
// Gated on `V = String` via a separate impl block. The methods accept
// `impl Into<String>` so callers can pass `&str`, `String`, or `Cow<str>`
// without an extra `.to_string()`. Non-string columns do not resolve these
// methods — the compiler reports "no method named `contains` on
// `FieldRef<M, i64>`", which is exactly the desired user experience.
impl<M: Model> FieldRef<M, String> {
    /// Case-insensitive substring match — SQL `ILIKE '%value%'`.
    /// Aliased as `icontains` for Django naming parity.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn contains(self, value: impl Into<String>) -> Condition {
        Condition::Leaf(Leaf {
            column: self.column,
            op: LookupOp::IContains,
            value: FilterValue::String(value.into()),
        })
    }

    /// Alias for `contains` — matches Django's `icontains` lookup name.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn icontains(self, value: impl Into<String>) -> Condition {
        self.contains(value)
    }

    /// Case-insensitive prefix match — SQL `ILIKE 'value%'`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn starts_with(self, value: impl Into<String>) -> Condition {
        Condition::Leaf(Leaf {
            column: self.column,
            op: LookupOp::IStartsWith,
            value: FilterValue::String(value.into()),
        })
    }

    /// Alias for `starts_with` — matches Django's `istartswith` lookup name.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn istarts_with(self, value: impl Into<String>) -> Condition {
        self.starts_with(value)
    }

    /// Case-insensitive suffix match — SQL `ILIKE '%value'`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn ends_with(self, value: impl Into<String>) -> Condition {
        Condition::Leaf(Leaf {
            column: self.column,
            op: LookupOp::IEndsWith,
            value: FilterValue::String(value.into()),
        })
    }

    /// Alias for `ends_with` — matches Django's `iendswith` lookup name.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn iends_with(self, value: impl Into<String>) -> Condition {
        self.ends_with(value)
    }

    /// POSIX regex match — SQL `column ~ value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn regex(self, value: impl Into<String>) -> Condition {
        Condition::Leaf(Leaf {
            column: self.column,
            op: LookupOp::Regex,
            value: FilterValue::String(value.into()),
        })
    }

    /// Case-insensitive POSIX regex — SQL `column ~* value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn iregex(self, value: impl Into<String>) -> Condition {
        Condition::Leaf(Leaf {
            column: self.column,
            op: LookupOp::IRegex,
            value: FilterValue::String(value.into()),
        })
    }
}

// ── NULL checks ───────────────────────────────────────────────────────────
//
// Apply to every `FieldRef` regardless of `V` — nullability is a
// column-level property, not a value-level one. Nullable columns in user
// structs are declared `Option<T>` and the macro still emits a
// `FieldRef<M, T>` (the inner type) so lookups remain ergonomic; these
// methods give the explicit NULL path.
impl<M: Model, V> FieldRef<M, V> {
    /// `column IS NULL`.
    ///
    /// **Applicability:** available on every `FieldRef<M, V>` regardless of
    /// whether the underlying column is declared `NOT NULL` in the schema.
    /// This is intentional — Postgres can produce a NULL for any column
    /// through outer joins, `COALESCE`, window functions over empty frames,
    /// or `CASE` expressions, so an `IS NULL` filter against a "non-nullable"
    /// base column is still a meaningful query in derived result sets. The
    /// type system deliberately does not gate this on `V = Option<T>`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn is_null(self) -> Condition {
        Condition::Leaf(Leaf {
            column: self.column,
            op: LookupOp::IsNull,
            value: FilterValue::Null,
        })
    }

    /// `column IS NOT NULL`.
    ///
    /// **Applicability:** available on every `FieldRef<M, V>` regardless of
    /// whether the underlying column is declared `NOT NULL` in the schema.
    /// This is intentional — Postgres can produce a NULL for any column
    /// through outer joins, `COALESCE`, window functions over empty frames,
    /// or `CASE` expressions, so an `IS NOT NULL` filter against a
    /// "non-nullable" base column is still a meaningful query in derived
    /// result sets. The type system deliberately does not gate this on
    /// `V = Option<T>`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn is_not_null(self) -> Condition {
        Condition::Leaf(Leaf {
            column: self.column,
            op: LookupOp::IsNotNull,
            value: FilterValue::Null,
        })
    }
}

// ── Fluent combinators on Condition ───────────────────────────────────────
//
// `Condition::and(a, b)` / `::or(a, b)` are the associative constructors in
// `query::condition`. `.and_with` / `.or_with` are the method-chain forms
// that read left-to-right in a filter closure:
//
// ```ignore
// f.title().eq("x").and_with(f.view_count().gte(100))
// ```
//
// Named `*_with` (rather than `.and` / `.or`) to avoid colliding with
// `bool::and` / `bool::or` if a future Condition impl adopts those names
// for short-circuit semantics.
impl Condition {
    /// Combine `self` and `other` with SQL `AND`. Fluent form of
    /// `Condition::and(self, other)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn and_with(self, other: Condition) -> Condition {
        Condition::and(self, other)
    }

    /// Combine `self` and `other` with SQL `OR`. Fluent form of
    /// `Condition::or(self, other)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn or_with(self, other: Condition) -> Condition {
        Condition::or(self, other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::condition::{Condition, LookupOp};

    // Test-local fake model — satisfies the `Model` trait enough to feed
    // `FieldRef`'s generics at compile time. Real integration tests against
    // Postgres live in `tests/integration/`; this file's unit tests cover
    // the `FieldRef` API in isolation.
    //
    // `manual_async_fn` is allowed because the `Model` trait signature uses
    // `-> impl Future + Send` explicitly (RPITIT) to match the stable form
    // real `#[model]`-generated impls emit. Converting to `async fn` would
    // change the trait signature, not just the impl. See `model.rs` for
    // the full rationale on `'a` lifetime + `+ Send` bound.
    struct Fake;
    impl crate::model::__sealed::Sealed for Fake {}
    #[allow(clippy::manual_async_fn)]
    impl crate::model::Model for Fake {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "fakes"
        }
        fn pk_value(&self) -> &i64 {
            unimplemented!()
        }
        fn descriptor() -> &'static crate::descriptor::ModelDescriptor {
            unimplemented!()
        }
        fn get<'a>(
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
            _id: i64,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn create<'a>(
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn save<'a>(
            &self,
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn delete<'a>(
            self,
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn refresh_from_db<'a>(
            &self,
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unimplemented!() }
        }
    }

    #[test]
    fn field_ref_eq_emits_leaf_with_eq_op() {
        let f: FieldRef<Fake, i64> = FieldRef::new("age");
        let c = f.eq(42i64);
        if let Condition::Leaf(leaf) = c {
            assert_eq!(leaf.column, "age");
            assert_eq!(leaf.op, LookupOp::Eq);
        } else {
            panic!("expected Leaf");
        }
    }

    #[test]
    fn field_ref_is_copy() {
        let f: FieldRef<Fake, String> = FieldRef::new("title");
        let g = f;
        let h = f;
        // Both uses of `f` post-move must compile because FieldRef: Copy.
        let _ = g.eq("a".to_string());
        let _ = h.neq("b".to_string());
    }

    #[test]
    fn field_ref_gte_emits_leaf_with_gte_op() {
        let f: FieldRef<Fake, i64> = FieldRef::new("view_count");
        let c = f.gte(100i64);
        if let Condition::Leaf(leaf) = c {
            assert_eq!(leaf.column, "view_count");
            assert_eq!(leaf.op, LookupOp::Gte);
        } else {
            panic!("expected Leaf");
        }
    }

    #[test]
    fn field_ref_between_emits_pair_filter_value() {
        let f: FieldRef<Fake, i64> = FieldRef::new("age");
        let c = f.between(10i64, 20i64);
        if let Condition::Leaf(leaf) = c {
            assert_eq!(leaf.op, LookupOp::Between);
            assert!(matches!(leaf.value, FilterValue::Pair(_, _)));
        } else {
            panic!("expected Leaf");
        }
    }

    #[test]
    fn field_ref_in_list_emits_list_filter_value() {
        let f: FieldRef<Fake, i64> = FieldRef::new("id");
        let c = f.in_list(vec![1i64, 2, 3]);
        if let Condition::Leaf(leaf) = c {
            assert_eq!(leaf.op, LookupOp::In);
            if let FilterValue::List(items) = &leaf.value {
                assert_eq!(items.len(), 3);
            } else {
                panic!("expected FilterValue::List");
            }
        } else {
            panic!("expected Leaf");
        }
    }

    #[test]
    fn field_ref_is_null_emits_null_filter_value() {
        let f: FieldRef<Fake, String> = FieldRef::new("deleted_at");
        let c = f.is_null();
        if let Condition::Leaf(leaf) = c {
            assert_eq!(leaf.op, LookupOp::IsNull);
            assert!(matches!(leaf.value, FilterValue::Null));
        } else {
            panic!("expected Leaf");
        }
    }

    #[test]
    fn field_ref_contains_string_only() {
        let f: FieldRef<Fake, String> = FieldRef::new("title");
        let c = f.contains("hello");
        if let Condition::Leaf(leaf) = c {
            assert_eq!(leaf.op, LookupOp::IContains);
        } else {
            panic!("expected Leaf");
        }
    }

    #[test]
    fn condition_and_with_chains_fluently() {
        let f: FieldRef<Fake, i64> = FieldRef::new("a");
        let g: FieldRef<Fake, i64> = FieldRef::new("b");
        let combined = f.eq(1i64).and_with(g.eq(2i64));
        if let Condition::And(parts) = combined {
            assert_eq!(parts.len(), 2);
        } else {
            panic!("expected And, got {combined:?}");
        }
    }

    #[test]
    fn condition_or_with_chains_fluently() {
        let f: FieldRef<Fake, i64> = FieldRef::new("a");
        let g: FieldRef<Fake, i64> = FieldRef::new("b");
        let combined = f.eq(1i64).or_with(g.eq(2i64));
        if let Condition::Or(parts) = combined {
            assert_eq!(parts.len(), 2);
        } else {
            panic!("expected Or, got {combined:?}");
        }
    }
}
