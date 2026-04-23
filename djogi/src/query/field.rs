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

use crate::jsonb::Jsonb;
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
    /// the `SqlAccumulator::push_sql` sites in `query::sql`. The macro
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

    /// Promote this column handle into the expression IR as
    /// [`Expr<V>`](crate::expr::Expr) — the entry point for
    /// field-vs-field comparisons, arithmetic composition, and (in
    /// later tasks) aggregates / subqueries / CASE.
    ///
    /// # Why a named method and not an `Into` impl?
    ///
    /// `FieldRef` already has typed lookup methods (`eq`, `neq`, …)
    /// that return [`Condition`] directly for the literal-RHS case.
    /// An `impl<M, V> From<FieldRef<M, V>> for Expr<V>` would make
    /// every `FieldRef` transparently coerce into `Expr<V>`, which is
    /// fine in isolation but would collide with future `Into` impls
    /// (for example, the `IntoAssignments` / `IntoDistinctColumns`
    /// bridges the Phase 2 API already ships). Keeping the promotion
    /// explicit — call sites read `f.balance.as_expr().lt(f.overdraft_limit.as_expr())` —
    /// also matches the Django / SeaORM idiom users are porting
    /// queries from.
    ///
    /// The `column` string has already been validated by
    /// [`crate::ident::assert_plain_ident`] at construction (see
    /// [`__macro_support::__make_field_ref`]); the SQL emitter in
    /// [`crate::expr::sql`] pushes it straight through `qb.push`
    /// without re-validation.
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn as_expr(self) -> crate::expr::Expr<V> {
        crate::expr::Expr::from_node(crate::expr::node::ExprNode::Field {
            column: self.column,
        })
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
/// into `SqlAccumulator::push_sql` inside `query::sql`'s
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
            fn get(
                _ctx: &mut crate::context::DjogiContext,
                _id: Self::Pk,
            ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
                async { unreachable!() }
            }
            fn create(
                _ctx: &mut crate::context::DjogiContext,
                _v: Self,
            ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
                async { unreachable!() }
            }
            fn save<'ctx>(
                &'ctx mut self,
                _ctx: &'ctx mut crate::context::DjogiContext,
            ) -> impl Future<Output = Result<(), DjogiError>> + Send + 'ctx {
                async { unreachable!() }
            }
            fn delete(
                self,
                _ctx: &mut crate::context::DjogiContext,
            ) -> impl Future<Output = Result<(), DjogiError>> + Send {
                async { unreachable!() }
            }
            fn refresh_from_db<'ctx>(
                &'ctx self,
                _ctx: &'ctx mut crate::context::DjogiContext,
            ) -> impl Future<Output = Result<Self, DjogiError>> + Send + 'ctx {
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
impl IntoFilterValue for rust_decimal::Decimal {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::Decimal(self)
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

// ── Array field operators ─────────────────────────────────────────────────
//
// Available on `FieldRef<M, Vec<V>>` for element types Djogi supports as
// Postgres array columns: `Vec<String>`, `Vec<i32>`, `Vec<i64>`, `Vec<bool>`.
//
// The sealed trait `IntoArrayFilterValue` maps each supported element type to
// its matching `FilterValue::Array*` variant. The sealed impl block prevents
// downstream crates from implementing the trait for unsupported types.

mod array_sealed {
    pub trait Sealed {}
    impl Sealed for String {}
    impl Sealed for i32 {}
    impl Sealed for i64 {}
    impl Sealed for bool {}
}

/// Converts a `Vec<V>` element type into the matching [`FilterValue::Array*`]
/// variant for use in array operator conditions.
///
/// Sealed so that only the Djogi-blessed array element types (`String`, `i32`,
/// `i64`, `bool`) can be used with the array operator methods on
/// `FieldRef<M, Vec<V>>`. Downstream code cannot implement this trait.
pub trait IntoArrayFilterValue: array_sealed::Sealed {
    /// Wrap a `Vec<Self>` in the corresponding `FilterValue::Array*` variant.
    fn into_array_filter_value(values: Vec<Self>) -> FilterValue
    where
        Self: Sized;
}

impl IntoArrayFilterValue for String {
    fn into_array_filter_value(values: Vec<Self>) -> FilterValue {
        FilterValue::ArrayString(values)
    }
}
impl IntoArrayFilterValue for i32 {
    fn into_array_filter_value(values: Vec<Self>) -> FilterValue {
        FilterValue::ArrayI32(values)
    }
}
impl IntoArrayFilterValue for i64 {
    fn into_array_filter_value(values: Vec<Self>) -> FilterValue {
        FilterValue::ArrayI64(values)
    }
}
impl IntoArrayFilterValue for bool {
    fn into_array_filter_value(values: Vec<Self>) -> FilterValue {
        FilterValue::ArrayBool(values)
    }
}

impl<M: Model, V: IntoArrayFilterValue + Clone + 'static> FieldRef<M, Vec<V>> {
    /// `column @> $1` — array contains.
    ///
    /// Returns rows where every element in `values` also appears in the column.
    /// Maps to the Postgres `@>` (contains) array operator.
    ///
    /// Djogi arrays are always 1-dimensional; multi-dimensional arrays are not
    /// a supported field type.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn contains(self, values: &[V]) -> Condition {
        Condition::ArrayContains(crate::array::ArrayContainsLeaf {
            column: self.column,
            values: V::into_array_filter_value(values.to_vec()),
        })
    }

    /// `column <@ $1` — contained by.
    ///
    /// Returns rows where every element in the column also appears in `values`.
    /// Maps to the Postgres `<@` (contained by) array operator.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn contained_by(self, values: &[V]) -> Condition {
        Condition::ArrayContainedBy(crate::array::ArrayContainedByLeaf {
            column: self.column,
            values: V::into_array_filter_value(values.to_vec()),
        })
    }

    /// `column && $1` — overlap (at least one element in common).
    ///
    /// Returns rows where the column and `values` share at least one element.
    /// Maps to the Postgres `&&` array overlap operator.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn overlap(self, values: &[V]) -> Condition {
        Condition::ArrayOverlap(crate::array::ArrayOverlapLeaf {
            column: self.column,
            values: V::into_array_filter_value(values.to_vec()),
        })
    }

    /// `array_length(column, 1)` — number of elements in the 1-dimensional array.
    ///
    /// Returns an [`Expr<i32>`](crate::expr::Expr) that slots into the
    /// expression IR so `.len().gt(3)` composes with the existing `Cmp`
    /// machinery (Phase 4 expression IR).
    ///
    /// The dimension argument is hardcoded to `1`. Djogi arrays are always
    /// 1-dimensional; multi-dimensional arrays are not a supported field type.
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn len(self) -> crate::expr::Expr<i32> {
        crate::expr::Expr::from_node(crate::expr::node::ExprNode::ArrayLength {
            column: self.column,
        })
    }
}

// ── JSONB flat-path entry point ───────────────────────────────────────────
//
// `.path::<V>("a.b.c")` on a `FieldRef<M, Jsonb<T>>` produces a
// `JsonbPathRef<M, V>` that exposes the same comparison surface
// (`eq`, `gt`, etc.) as a plain `FieldRef<M, V>`, but emits
// `(col->'a'->'b'->>'c')::cast op $n` instead of `col op $n`.

impl<M: Model, T> FieldRef<M, Jsonb<T>> {
    /// Navigate to a sub-field of this JSONB column via a dot-separated path.
    ///
    /// Returns a [`JsonbPathRef<M, V>`](crate::jsonb::JsonbPathRef) that supports
    /// the same comparison surface as `FieldRef<M, V>`: `eq`, `neq`, `gt`,
    /// `gte`, `lt`, `lte`, `in_list`, `is_null`, `is_not_null`.
    ///
    /// # Path format
    ///
    /// Dot-separated segments. Each segment must be non-empty, begin with an
    /// ASCII letter or underscore, contain only ASCII alphanumerics or
    /// underscores, and be at most 63 bytes long.
    ///
    /// # SQL emission
    ///
    /// `specs.path::<i32>("engine.cylinders")` emits
    /// `(specs->'engine'->>'cylinders')::int` on the LHS of the comparison.
    ///
    /// # Example
    ///
    /// ```ignore
    /// Post::objects()
    ///     .filter(|f| f.specs().path::<i32>("engine.cylinders").gt(4))
    ///     .fetch_all(&mut ctx).await?
    /// ```
    #[must_use = "JsonbPathRef is lazy — dropping one silently omits the filter"]
    pub fn path<V>(self, dotted: &'static str) -> crate::jsonb::JsonbPathRef<M, V> {
        crate::jsonb::JsonbPathRef::new(self.column, dotted)
    }

    /// Enter the compile-time typed path tree for this JSONB column.
    ///
    /// Returns `T::Path<M>` — the derive-generated tree that provides one
    /// method per field of `T`. Scalar fields return a
    /// [`JsonbPathRef<M, V>`](crate::jsonb::JsonbPathRef) ready for
    /// comparison; nested fields return the nested type's `Path<M>`.
    ///
    /// `T` must implement [`JsonbSchema`](crate::jsonb::JsonbSchema) (done
    /// by adding `#[derive(JsonbSchema)]` to the schema struct). If `T`
    /// does not implement `JsonbSchema` the compiler reports a trait-bound
    /// error at the `#[derive(Model)]` site.
    ///
    /// The flat [`path`](Self::path) escape hatch remains available when you
    /// need dynamic paths or types outside the cast-matrix allowlist.
    ///
    /// # Example
    ///
    /// ```ignore
    /// Vehicle::objects()
    ///     .filter(|f| f.specs().typed().engine.cylinders.gt(4))
    ///     .fetch_all(&mut ctx).await?
    /// ```
    #[must_use = "typed path handles are lazy — dropping one silently omits the filter"]
    pub fn typed(self) -> T::Path<M>
    where
        T: crate::jsonb::JsonbSchema,
    {
        T::root_path::<M>(self.column)
    }
}

/// `path()` is also available on nullable JSONB columns (`Option<Jsonb<T>>`).
/// The SQL expression navigates into the column as if it were non-null; when
/// the column IS NULL the Postgres JSONB path operators themselves return NULL,
/// so comparisons naturally exclude NULL rows — consistent with SQL's
/// three-valued logic.
impl<M: Model, T> FieldRef<M, Option<Jsonb<T>>> {
    /// Navigate to a sub-field of this nullable JSONB column.
    ///
    /// Rows where the column IS NULL are excluded by the comparison (SQL NULL
    /// semantics), which is the expected behavior for optional JSONB fields.
    ///
    /// See [`FieldRef<M, Jsonb<T>>::path`] for the full documentation.
    #[must_use = "JsonbPathRef is lazy — dropping one silently omits the filter"]
    pub fn path<V>(self, dotted: &'static str) -> crate::jsonb::JsonbPathRef<M, V> {
        crate::jsonb::JsonbPathRef::new(self.column, dotted)
    }

    /// Enter the compile-time typed path tree for this nullable JSONB column.
    ///
    /// Rows where the column IS NULL are excluded by comparisons (SQL NULL
    /// semantics). Otherwise identical to the non-nullable variant —
    /// see [`FieldRef<M, Jsonb<T>>::typed`].
    #[must_use = "typed path handles are lazy — dropping one silently omits the filter"]
    pub fn typed(self) -> T::Path<M>
    where
        T: crate::jsonb::JsonbSchema,
    {
        T::root_path::<M>(self.column)
    }
}

// ── Spatial operators on GeoPoint fields (Phase 6 `spatial` feature) ────────
//
// Gated on `#[cfg(feature = "spatial")]`. Available only on
// `FieldRef<M, GeoPoint>`, so calling `.within_km(...)` on a non-spatial
// column yields a helpful "no method named `within_km`" compile error — the
// type system is the documentation, same as the existing String-only lookup
// methods above.

#[cfg(feature = "spatial")]
impl<M: crate::model::Model> FieldRef<M, crate::geo::GeoPoint> {
    /// Filter rows where this geography column is within `km` kilometers of
    /// `center`.
    ///
    /// # SQL emission
    ///
    /// Emits `ST_DWithin(<col>, ST_Point($lon, $lat)::geography, $r)` where:
    ///
    /// - `$lon` and `$lat` are the longitude and latitude of `center` bound
    ///   as parameters.
    /// - `$r` is `km * 1000.0` (the radius in meters) bound as a parameter.
    ///
    /// The radius is converted from kilometers to meters internally so the bind
    /// type matches `ST_DWithin`'s `GEOGRAPHY` distance-in-meters signature.
    ///
    /// All three values flow through `push_bind` — no string interpolation of
    /// user-supplied data.
    ///
    /// # Example
    ///
    /// ```ignore
    /// Place::objects()
    ///     .filter(|p| p.location().within_km(
    ///         GeoPoint::new(37.7749, -122.4194).unwrap(),
    ///         50.0,  // km
    ///     ))
    ///     .fetch_all(&mut ctx).await?
    /// ```
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn within_km(self, center: crate::geo::GeoPoint, km: f64) -> Condition {
        use crate::expr::node::ExprNode;
        use crate::expr::spatial::SpatialExpr;
        Condition::Expr(crate::expr::Expr::from_node(ExprNode::Spatial(
            SpatialExpr::Within {
                field_column: self.column(),
                center,
                radius_meters: km * 1000.0,
            },
        )))
    }

    /// Order rows by ascending distance from `center`, with the model's primary
    /// key appended as a deterministic tiebreaker.
    ///
    /// # Why the tiebreak?
    ///
    /// Without the tiebreak, equidistant rows return in arbitrary Postgres order
    /// — flaky tests and inconsistent pagination cursors. The primary-key
    /// tiebreak is always appended unconditionally. Callers who chain additional
    /// `.order_by(...)` calls get their keys appended after the tiebreak.
    ///
    /// # SQL emission
    ///
    /// Emits two comma-separated ORDER BY terms:
    /// ```sql
    /// ST_Distance(<col>, ST_Point($lon, $lat)::geography) ASC, id ASC
    /// ```
    ///
    /// `$lon` and `$lat` are bound as parameters. The `id` column name is
    /// the model's primary key — captured from `M::descriptor().pk_column()`
    /// at call time.
    ///
    /// # Example
    ///
    /// ```ignore
    /// Place::objects()
    ///     .order_by(|p| p.location().order_by_distance(
    ///         GeoPoint::new(37.7749, -122.4194).unwrap()
    ///     ))
    ///     .fetch_all(&mut ctx).await?
    /// ```
    #[must_use = "order expressions are inert until passed to `order_by`"]
    pub fn order_by_distance(self, center: crate::geo::GeoPoint) -> crate::query::order::OrderExpr {
        // Get the PK column from the descriptor. For all standard PK types
        // (HeerId, RanjId, Serial) this is "id".
        //
        // The `unwrap_or(self.column())` fallback path is defensive-only and
        // unreachable from the public API: `FieldRef<M, GeoPoint>` requires
        // `M: Model`, and `#[model(pk = "none")]` models do not receive a
        // `Model` impl from the macro (only `pk = "none"` with no ordering
        // semantics), so they cannot reach the spatial query surface at all.
        let pk_column = M::descriptor().pk_column().unwrap_or(self.column());
        crate::query::order::OrderExpr::spatial_distance_with_pk_tiebreak(
            self.column(),
            center,
            pk_column,
        )
    }
}

// ── Spatial shape predicates on any GeographyValue field (T9) ────────────────
//
// Gated on `#[cfg(feature = "spatial")]`. Generic over `G: GeographyValue` so
// the methods are available on `FieldRef<M, Polygon>`, `FieldRef<M, LineString>`,
// `FieldRef<M, GeoPoint>`, etc. The `other` argument is also generic (`O:
// GeographyValue`) so callers may test across geometry types — for example,
// `FieldRef<M, Polygon>::intersects(some_linestring)` is valid because both
// `Polygon` and `LineString` implement `GeographyValue`.
//
// Note: `.within_km` remains in the `impl<M: Model> FieldRef<M, GeoPoint>`
// block above — it is radius-based and specific to `GeoPoint`. The shape-based
// `.within(&geom)` below lives here (generic receiver) and routes to the
// `WithinShape` variant, avoiding any naming collision.

#[cfg(feature = "spatial")]
impl<M: crate::model::Model, G: crate::geo::GeographyValue> FieldRef<M, G> {
    /// Filter rows where this geography column entirely contains `other`.
    ///
    /// # SQL emission
    ///
    /// Emits `ST_Contains(<col>, $1::geography)` where `$1` is the EWKB
    /// encoding of `other`. The EWKB bytes flow through `push_bind` — no
    /// string interpolation of user-supplied data.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Find delivery zones that fully contain the customer's neighbourhood.
    /// DeliveryZone::objects()
    ///     .filter(|z| z.area().contains(&neighbourhood_polygon))
    ///     .fetch_all(&mut ctx).await?
    /// ```
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn contains<O: crate::geo::GeographyValue>(
        self,
        other: &O,
    ) -> crate::query::condition::Condition {
        use crate::expr::node::ExprNode;
        use crate::expr::spatial::SpatialExpr;
        crate::query::condition::Condition::Expr(crate::expr::Expr::from_node(ExprNode::Spatial(
            SpatialExpr::Contains {
                field_column: self.column(),
                other_ewkb: other.to_ewkb_bytes(),
            },
        )))
    }

    /// Filter rows where this geography column intersects `other` (shares at
    /// least one point).
    ///
    /// # SQL emission
    ///
    /// Emits `ST_Intersects(<col>, $1::geography)` where `$1` is the EWKB
    /// encoding of `other`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Find routes that cross the construction zone.
    /// Route::objects()
    ///     .filter(|r| r.path().intersects(&construction_zone))
    ///     .fetch_all(&mut ctx).await?
    /// ```
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn intersects<O: crate::geo::GeographyValue>(
        self,
        other: &O,
    ) -> crate::query::condition::Condition {
        use crate::expr::node::ExprNode;
        use crate::expr::spatial::SpatialExpr;
        crate::query::condition::Condition::Expr(crate::expr::Expr::from_node(ExprNode::Spatial(
            SpatialExpr::Intersects {
                field_column: self.column(),
                other_ewkb: other.to_ewkb_bytes(),
            },
        )))
    }

    /// Filter rows where this geography column touches `other` — the geometries
    /// share boundary points but no interior points (touch but do not overlap).
    ///
    /// # SQL emission
    ///
    /// Emits `ST_Touches(<col>, $1::geography)` where `$1` is the EWKB
    /// encoding of `other`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Find parcels adjacent to (touching) the road boundary.
    /// Parcel::objects()
    ///     .filter(|p| p.boundary().touches(&road_line))
    ///     .fetch_all(&mut ctx).await?
    /// ```
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn touches<O: crate::geo::GeographyValue>(
        self,
        other: &O,
    ) -> crate::query::condition::Condition {
        use crate::expr::node::ExprNode;
        use crate::expr::spatial::SpatialExpr;
        crate::query::condition::Condition::Expr(crate::expr::Expr::from_node(ExprNode::Spatial(
            SpatialExpr::Touches {
                field_column: self.column(),
                other_ewkb: other.to_ewkb_bytes(),
            },
        )))
    }

    /// Filter rows where this geography column is entirely within `other`.
    ///
    /// This is the shape-based `within` — distinct from Phase 6's radius-based
    /// `.within_km(center, km)` on `FieldRef<M, GeoPoint>`. The two methods
    /// live on different receivers and do not collide.
    ///
    /// # SQL emission
    ///
    /// Emits `ST_Within(<col>, $1::geography)` where `$1` is the EWKB
    /// encoding of `other`. Internally routes to `SpatialExpr::WithinShape`
    /// to avoid the variant-name collision with the radius-based `Within`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Find deliveries whose drop-off point falls inside a coverage polygon.
    /// Delivery::objects()
    ///     .filter(|d| d.drop_off().within(&coverage_polygon))
    ///     .fetch_all(&mut ctx).await?
    /// ```
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn within<O: crate::geo::GeographyValue>(
        self,
        other: &O,
    ) -> crate::query::condition::Condition {
        use crate::expr::node::ExprNode;
        use crate::expr::spatial::SpatialExpr;
        crate::query::condition::Condition::Expr(crate::expr::Expr::from_node(ExprNode::Spatial(
            SpatialExpr::WithinShape {
                field_column: self.column(),
                other_ewkb: other.to_ewkb_bytes(),
            },
        )))
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
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: i64,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send + 'ctx
        {
            async { unimplemented!() }
        }
        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send + 'ctx
        {
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

// ── T9: Method dispatch tests for shape predicates ────────────────────────

#[cfg(all(test, feature = "spatial"))]
mod spatial_field_tests {
    use super::*;
    use crate::expr::node::ExprNode;
    use crate::expr::spatial::SpatialExpr;
    use crate::geo::{GeoPoint, LineString, MultiPoint, MultiPolygon, Polygon};
    use crate::query::condition::Condition;
    use std::future::Future;

    // Minimal `Model` stub for spatial method dispatch tests.
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
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: i64,
        ) -> impl Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl Future<Output = Result<(), crate::DjogiError>> + Send + 'ctx {
            async { unimplemented!() }
        }
        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl Future<Output = Result<Self, crate::DjogiError>> + Send + 'ctx {
            async { unimplemented!() }
        }
    }

    // Helper: build a minimal valid `Polygon` using the `closed` constructor.
    fn make_polygon() -> Polygon {
        let ring = [
            GeoPoint::new(0.0, 0.0).unwrap(),
            GeoPoint::new(1.0, 0.0).unwrap(),
            GeoPoint::new(1.0, 1.0).unwrap(),
            GeoPoint::new(0.0, 1.0).unwrap(),
            GeoPoint::new(0.0, 0.0).unwrap(), // closed ring
        ];
        Polygon::closed(&ring).unwrap()
    }

    // Helper: extract the `SpatialExpr` from a `Condition::Expr(Expr<bool>)`.
    fn unwrap_spatial(cond: Condition) -> SpatialExpr {
        if let Condition::Expr(expr) = cond
            && let ExprNode::Spatial(s) = expr.node
        {
            return s;
        }
        panic!("expected Condition::Expr(ExprNode::Spatial(...))");
    }

    // ── contains ─────────────────────────────────────────────────────────────

    /// `.contains(&poly)` on a `FieldRef<Fake, Polygon>` must produce
    /// `Condition::Expr(ExprNode::Spatial(SpatialExpr::Contains { .. }))`.
    #[test]
    fn contains_method_dispatch_produces_contains_variant() {
        let poly = make_polygon();
        let field: FieldRef<Fake, Polygon> = FieldRef::new("area");
        let cond = field.contains(&poly);
        let s = unwrap_spatial(cond);
        assert!(
            matches!(
                s,
                SpatialExpr::Contains {
                    field_column: "area",
                    ..
                }
            ),
            "expected Contains variant, got {s:?}"
        );
    }

    /// `.contains` injection safety: EWKB bytes must not appear as literal SQL.
    #[test]
    fn contains_method_dispatch_ewkb_is_bound() {
        use crate::pg::accumulator::SqlAccumulator;
        let poly = make_polygon();
        let field: FieldRef<Fake, Polygon> = FieldRef::new("area");
        let cond = field.contains(&poly);
        let s = unwrap_spatial(cond);
        let mut acc = SqlAccumulator::new("");
        s.emit(&mut acc);
        assert_eq!(acc.bind_count(), 1, "EWKB must flow through push_bind");
        assert!(acc.sql().contains("$1"), "expected $1 placeholder");
    }

    // ── intersects ────────────────────────────────────────────────────────────

    /// `.intersects(&line)` on a `FieldRef<Fake, LineString>` must produce
    /// `SpatialExpr::Intersects { field_column: "route", .. }`.
    #[test]
    fn intersects_method_dispatch_produces_intersects_variant() {
        let pts = [
            GeoPoint::new(0.0, 0.0).unwrap(),
            GeoPoint::new(1.0, 1.0).unwrap(),
        ];
        let line = LineString::new(&pts).unwrap();
        let field: FieldRef<Fake, LineString> = FieldRef::new("route");
        let cond = field.intersects(&line);
        let s = unwrap_spatial(cond);
        assert!(
            matches!(
                s,
                SpatialExpr::Intersects {
                    field_column: "route",
                    ..
                }
            ),
            "expected Intersects variant, got {s:?}"
        );
    }

    /// Cross-geometry dispatch: `FieldRef<Fake, Polygon>::intersects(some_linestring)`.
    /// Both types implement `GeographyValue`; the method must accept the call.
    #[test]
    fn intersects_cross_geometry_polygon_field_with_linestring_arg() {
        let pts = [
            GeoPoint::new(0.0, 0.0).unwrap(),
            GeoPoint::new(2.0, 2.0).unwrap(),
        ];
        let line = LineString::new(&pts).unwrap();
        let field: FieldRef<Fake, Polygon> = FieldRef::new("area");
        // `Polygon` field, `LineString` argument — both are `GeographyValue`.
        let cond = field.intersects(&line);
        let s = unwrap_spatial(cond);
        assert!(
            matches!(
                s,
                SpatialExpr::Intersects {
                    field_column: "area",
                    ..
                }
            ),
            "cross-geometry intersects failed: {s:?}"
        );
    }

    // ── touches ───────────────────────────────────────────────────────────────

    /// `.touches(&poly)` on a `FieldRef<Fake, Polygon>` must produce
    /// `SpatialExpr::Touches { field_column: "boundary", .. }`.
    #[test]
    fn touches_method_dispatch_produces_touches_variant() {
        let poly = make_polygon();
        let field: FieldRef<Fake, Polygon> = FieldRef::new("boundary");
        let cond = field.touches(&poly);
        let s = unwrap_spatial(cond);
        assert!(
            matches!(
                s,
                SpatialExpr::Touches {
                    field_column: "boundary",
                    ..
                }
            ),
            "expected Touches variant, got {s:?}"
        );
    }

    /// `.touches` injection safety.
    #[test]
    fn touches_method_dispatch_ewkb_is_bound() {
        use crate::pg::accumulator::SqlAccumulator;
        let poly = make_polygon();
        let field: FieldRef<Fake, Polygon> = FieldRef::new("boundary");
        let cond = field.touches(&poly);
        let s = unwrap_spatial(cond);
        let mut acc = SqlAccumulator::new("");
        s.emit(&mut acc);
        assert_eq!(acc.bind_count(), 1);
        assert!(acc.sql().contains("$1"));
    }

    // ── within ────────────────────────────────────────────────────────────────

    /// `.within(&poly)` on a `FieldRef<Fake, GeoPoint>` must produce
    /// `SpatialExpr::WithinShape { field_column: "drop_off", .. }`.
    #[test]
    fn within_method_dispatch_produces_within_shape_variant() {
        let poly = make_polygon();
        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("drop_off");
        let cond = field.within(&poly);
        let s = unwrap_spatial(cond);
        assert!(
            matches!(
                s,
                SpatialExpr::WithinShape {
                    field_column: "drop_off",
                    ..
                }
            ),
            "expected WithinShape variant, got {s:?}"
        );
    }

    /// `.within` injection safety.
    #[test]
    fn within_method_dispatch_ewkb_is_bound() {
        use crate::pg::accumulator::SqlAccumulator;
        let poly = make_polygon();
        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("drop_off");
        let cond = field.within(&poly);
        let s = unwrap_spatial(cond);
        let mut acc = SqlAccumulator::new("");
        s.emit(&mut acc);
        assert_eq!(acc.bind_count(), 1);
        assert!(acc.sql().contains("$1"));
    }

    /// Cross-geometry dispatch: `FieldRef<Fake, MultiPolygon>::within(some_multipolygon)`.
    /// Both the field type and arg type are `GeographyValue`.
    #[test]
    fn within_cross_geometry_multipolygon_field() {
        let poly = make_polygon();
        let mpoly = MultiPolygon::new(vec![poly]).unwrap();
        let field: FieldRef<Fake, MultiPolygon> = FieldRef::new("coverage");
        let cond = field.within(&mpoly);
        let s = unwrap_spatial(cond);
        assert!(
            matches!(
                s,
                SpatialExpr::WithinShape {
                    field_column: "coverage",
                    ..
                }
            ),
            "cross-geometry within failed: {s:?}"
        );
    }

    /// Cross-geometry dispatch: `FieldRef<Fake, GeoPoint>::intersects(multipoint)`.
    #[test]
    fn contains_cross_geometry_geopoint_field_multipoint_arg() {
        let pts = [GeoPoint::new(0.0, 0.0).unwrap()];
        let mpt = MultiPoint::new(&pts).unwrap();
        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("loc");
        let cond = field.intersects(&mpt);
        let s = unwrap_spatial(cond);
        assert!(
            matches!(
                s,
                SpatialExpr::Intersects {
                    field_column: "loc",
                    ..
                }
            ),
            "expected Intersects, got {s:?}"
        );
    }
}
