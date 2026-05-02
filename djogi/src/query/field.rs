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

    /// Construct a [`FieldRef<M, V>`] from a macro-emitted column name,
    /// with an optional SQL alias path prefix.
    ///
    /// - `prefix = None` — plain column reference, e.g. `"name"`.
    /// - `prefix = Some("department")` + `column = "name"` → produces a
    ///   `FieldRef` whose column string is `"department.name"`.
    ///
    /// The only supported caller is the `{Model}Fields` / `{Visage}Fields`
    /// accessor emitted by `#[derive(Model)]` in the user's crate.
    ///
    /// Panics if `column` (or `prefix`, when `Some`) violates any rule in
    /// [`crate::ident::assert_plain_ident`]: empty, over 63 bytes,
    /// leading digit, a non-identifier byte, or a reserved Postgres
    /// keyword.
    ///
    /// # Composed-path interning
    ///
    /// When `prefix` is `Some`, runtime path composition produces a
    /// `String`; Djogi's emission contract demands `&'static str`. The
    /// first `(prefix, column)` pair `Box::leak`s the composite; every
    /// subsequent call for the same pair returns the already-leaked
    /// `&'static str`. Bound is `O(distinct (prefix, column) pairs in
    /// the adopter's schema)` — a few dozen entries for a real app,
    /// regardless of request load. The intern map is
    /// `OnceLock<Mutex<HashSet<&'static str>>>`; lookups are O(1) and
    /// the lock is uncontended in the steady state.
    #[doc(hidden)]
    pub fn __make_field_ref<M: Model, V>(
        prefix: Option<&'static str>,
        column: &'static str,
    ) -> FieldRef<M, V> {
        assert_plain_ident(column, "field_column");
        let resolved = match prefix {
            Some(p) => {
                assert_plain_ident(p, "field_path_prefix");
                intern_composed_path(p, column)
            }
            None => column,
        };
        FieldRef::new(resolved)
    }

    /// Intern a freshly-composed `"{prefix}.{column}"` path and return
    /// the cached `&'static str` handle. First observation of a given
    /// `(prefix, column)` pair `Box::leak`s the composite; every later
    /// call returns the same reference. Thread-safe.
    fn intern_composed_path(prefix: &'static str, column: &'static str) -> &'static str {
        use std::collections::HashSet;
        use std::sync::{Mutex, OnceLock};

        static INTERN: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();

        let set_mutex = INTERN.get_or_init(|| Mutex::new(HashSet::new()));
        // Build the candidate composite on the heap. The allocation is
        // unavoidable — we need a `String` to hash against the set. The
        // leak only happens when the candidate is a first observation.
        let candidate = format!("{prefix}.{column}");
        let mut set = set_mutex.lock().expect("field-path intern mutex poisoned");
        if let Some(existing) = set.get(candidate.as_str()) {
            return existing;
        }
        let leaked: &'static str = Box::leak(candidate.into_boxed_str());
        set.insert(leaked);
        leaked
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
            std::panic::catch_unwind(|| __make_field_ref::<M, String>(None, column))
        }

        fn try_make_with_prefix(
            prefix: &'static str,
            column: &'static str,
        ) -> std::thread::Result<FieldRef<M, String>> {
            std::panic::catch_unwind(|| __make_field_ref::<M, String>(Some(prefix), column))
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

        #[test]
        fn prefix_composes_dot_qualified_path() {
            let r = try_make_with_prefix("department", "name");
            assert!(r.is_ok());
            assert_eq!(r.unwrap().column(), "department.name");
        }

        #[test]
        fn prefix_validates_prefix_segment() {
            // A bad prefix (reserved keyword) must still be rejected.
            assert!(try_make_with_prefix("select", "name").is_err());
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
// Narrow integer widening (Phase 7-Zero-2 polish, GH issue #29).
//
// Postgres has no native unsigned-integer types and no `i8`. Adopters
// who model fields as `u8` / `u16` / `u32` / `i8` (port numbers, small
// counts, signed-byte audio samples, etc.) need to compare against
// those values without manually upcasting. Each narrow type widens to
// the smallest signed Postgres type that fits its full range:
//
// - `i8`  → `I16` (smallint)   — i8 fits in int2 directly.
// - `u8`  → `I16` (smallint)   — u8 max 255 fits in int2's 32_767.
// - `u16` → `I32` (integer)    — u16 max 65_535 exceeds i16's 32_767.
// - `u32` → `I64` (bigint)     — u32 max ~4.3B exceeds i32's ~2.1B.
//
// `u64` deliberately has no impl: u64 max (~18.4 quintillion) exceeds
// i64 max (~9.2 quintillion). Adopters who genuinely need `u64`
// values bind via `numeric` through `rust_decimal::Decimal` instead.
impl IntoFilterValue for i8 {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::I16(i16::from(self))
    }
}
impl IntoFilterValue for u8 {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::I16(i16::from(self))
    }
}
impl IntoFilterValue for u16 {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::I32(i32::from(self))
    }
}
impl IntoFilterValue for u32 {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::I64(i64::from(self))
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
impl IntoFilterValue for crate::HeerIdDesc {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::HeerIdDesc(self)
    }
}
impl IntoFilterValue for crate::RanjIdDesc {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::RanjIdDesc(self)
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
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::Eq,
            value.into_filter_value(),
        ))
    }

    /// `column <> value` — SQL inequality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn neq(self, value: V) -> Condition {
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::Neq,
            value.into_filter_value(),
        ))
    }

    /// `column > value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn gt(self, value: V) -> Condition {
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::Gt,
            value.into_filter_value(),
        ))
    }

    /// `column >= value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn gte(self, value: V) -> Condition {
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::Gte,
            value.into_filter_value(),
        ))
    }

    /// `column < value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn lt(self, value: V) -> Condition {
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::Lt,
            value.into_filter_value(),
        ))
    }

    /// `column <= value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn lte(self, value: V) -> Condition {
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::Lte,
            value.into_filter_value(),
        ))
    }

    /// `column BETWEEN a AND b` (inclusive on both ends per SQL spec).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn between(self, a: V, b: V) -> Condition {
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::Between,
            FilterValue::Pair(
                Box::new(a.into_filter_value()),
                Box::new(b.into_filter_value()),
            ),
        ))
    }

    /// Case-insensitive equality — `LOWER(column) = LOWER(value)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn iexact(self, value: V) -> Condition {
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::IExact,
            value.into_filter_value(),
        ))
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
        Condition::Leaf(Leaf::new(self.column, LookupOp::In, list))
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
        Condition::Leaf(Leaf::new(self.column, LookupOp::NotIn, list))
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
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::IContains,
            FilterValue::String(value.into()),
        ))
    }

    /// Alias for `contains` — matches Django's `icontains` lookup name.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn icontains(self, value: impl Into<String>) -> Condition {
        self.contains(value)
    }

    /// Case-insensitive prefix match — SQL `ILIKE 'value%'`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn starts_with(self, value: impl Into<String>) -> Condition {
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::IStartsWith,
            FilterValue::String(value.into()),
        ))
    }

    /// Alias for `starts_with` — matches Django's `istartswith` lookup name.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn istarts_with(self, value: impl Into<String>) -> Condition {
        self.starts_with(value)
    }

    /// Case-insensitive suffix match — SQL `ILIKE '%value'`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn ends_with(self, value: impl Into<String>) -> Condition {
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::IEndsWith,
            FilterValue::String(value.into()),
        ))
    }

    /// Alias for `ends_with` — matches Django's `iendswith` lookup name.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn iends_with(self, value: impl Into<String>) -> Condition {
        self.ends_with(value)
    }

    /// Filter rows where the column matches `value` under Postgres's
    /// POSIX regex operator (`column ~ $1`, case-sensitive).
    ///
    /// `value` is a Postgres POSIX regex pattern — *not* a PCRE pattern,
    /// and *not* a Rust regex pattern. The match is performed entirely
    /// server-side; Djogi does not link a Rust regex engine, and the
    /// `regex` rule in `docs/spec/decisions.md` deliberately carves out
    /// Postgres-side `~` / `~*` because the operator is a Postgres
    /// feature exposed through the typed query API.
    ///
    /// For literal-substring matching, prefer
    /// [`contains`](Self::contains) — it escapes `%`, `_`, and `\` for
    /// you. Reach for `regex` only when the predicate genuinely needs
    /// alternation, anchors, or character classes.
    ///
    /// Postgres POSIX regex syntax is documented in the Postgres manual
    /// (§ "Pattern Matching", "POSIX Regular Expressions"). It differs
    /// from PCRE in several places (anchoring, lookaround support,
    /// quoting); patterns from other engines may not transfer.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn regex(self, value: impl Into<String>) -> Condition {
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::Regex,
            FilterValue::String(value.into()),
        ))
    }

    /// Case-insensitive sibling of [`regex`](Self::regex) — SQL
    /// `column ~* $1`. Same Postgres-feature framing applies: no Rust
    /// regex engine is involved, the match runs entirely server-side.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn iregex(self, value: impl Into<String>) -> Condition {
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::IRegex,
            FilterValue::String(value.into()),
        ))
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
        Condition::Leaf(Leaf::new(self.column, LookupOp::IsNull, FilterValue::Null))
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
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::IsNotNull,
            FilterValue::Null,
        ))
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
        // `M: Model`, and `#[model(pk = None)]` models do not receive a
        // `Model` impl from the macro (only `pk = None` with no ordering
        // semantics), so they cannot reach the spatial query surface at all.
        let pk_column = M::descriptor().pk_column().unwrap_or(self.column());
        crate::query::order::OrderExpr::spatial_distance_with_pk_tiebreak(
            self.column(),
            center,
            pk_column,
        )
    }
}

// ── Spatial operators on Option<GeoPoint> fields (#16 closure) ──────────────
//
// Mirrors the `FieldRef<M, GeoPoint>` block above for the nullable
// variant. Adopters who model location as `Option<GeoPoint>` (the
// natural Rust shape for "may not be located yet") can call the same
// `.within_km` / `.order_by_distance` methods directly:
//
// - `within_km` gates the spatial predicate behind a sibling
//   `IS NOT NULL` check, AND-combined with the existing
//   `ST_DWithin(...)` predicate. Postgres also drops NULL-geo rows
//   via three-valued logic on `ST_DWithin` (NULL ⇒ false in WHERE),
//   but the explicit guard makes the contract loud at the emission
//   layer and matches the issue's "raw SQL with hand-written IS NOT
//   NULL" workaround pattern adopters were using.
// - `order_by_distance` delegates directly to the non-Option impl.
//   Postgres's default `NULL` handling for ASC ordering is `NULLS
//   LAST`, so NULL-geo rows already sink to the end of the result
//   without needing an explicit `NULLS LAST` clause. PK tiebreak
//   still applies after distance for deterministic equidistant
//   ordering.
//
// SQL parity with the non-nullable variant means callers can swap
// `GeoPoint` for `Option<GeoPoint>` at the schema level without
// changing the query call sites — exactly the ergonomic the issue
// asked for.

#[cfg(feature = "spatial")]
impl<M: crate::model::Model> FieldRef<M, ::std::option::Option<crate::geo::GeoPoint>> {
    /// Filter rows where this nullable geography column is within
    /// `km` kilometers of `center`. Rows whose column is NULL are
    /// excluded by an explicit `IS NOT NULL` guard AND-combined
    /// with the underlying `ST_DWithin(...)` predicate.
    ///
    /// SQL: `<col> IS NOT NULL AND ST_DWithin(<col>, ST_Point($lon, $lat)::geography, $r)`.
    ///
    /// See [`FieldRef<M, GeoPoint>::within_km`] for the non-nullable
    /// variant and the parameter-binding details — the inner
    /// `ST_DWithin` shape is identical.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn within_km(self, center: crate::geo::GeoPoint, km: f64) -> Condition {
        // Build the typed `IS NOT NULL` guard via the generic helper
        // available on every FieldRef.
        let guard: FieldRef<M, ::std::option::Option<crate::geo::GeoPoint>> =
            FieldRef::new(self.column);
        let is_not_null = guard.is_not_null();
        // Lift the column into a non-Option FieldRef so we reuse the
        // same SpatialExpr emission as the non-nullable path.
        let inner: FieldRef<M, crate::geo::GeoPoint> = FieldRef::new(self.column);
        Condition::and(is_not_null, inner.within_km(center, km))
    }

    /// Order rows by ascending distance from `center`. NULL-geo rows
    /// fall to the end of the result via Postgres's default ASC NULL
    /// handling (`NULLS LAST` is the documented default — no explicit
    /// clause emitted). PK tiebreak still applies after distance for
    /// deterministic equidistant ordering.
    ///
    /// SQL:
    /// ```sql
    /// ST_Distance(<col>, ST_Point($lon, $lat)::geography) ASC, id ASC
    /// ```
    #[must_use = "order expressions are inert until passed to `order_by`"]
    pub fn order_by_distance(self, center: crate::geo::GeoPoint) -> crate::query::order::OrderExpr {
        let inner: FieldRef<M, crate::geo::GeoPoint> = FieldRef::new(self.column);
        inner.order_by_distance(center)
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

// ── T10: bounded_by (any GeographyValue) + distance_to (GeoPoint-only) ──────
//
// `.bounded_by` is generic over `G: GeographyValue` — a bbox prefilter makes
// sense for any geography column (polygon coverage zones, linestring routes,
// point locations, etc.). The four coordinate arguments follow the GeoPoint
// (lat, lon) convention; emission swaps to Postgres (x=lon, y=lat) order.
//
// `.distance_to` is specific to `FieldRef<M, GeoPoint>` because
// `ST_Distance` applied to a non-point geometry (e.g. a Polygon) returns the
// minimum boundary distance, which is a different semantic. Keeping it on the
// GeoPoint receiver makes the API unambiguous and mirrors the existing
// `.within_km` / `.order_by_distance` surface.

#[cfg(feature = "spatial")]
impl<M: crate::model::Model, G: crate::geo::GeographyValue> FieldRef<M, G> {
    /// Emits a GiST-indexed bounding-box prefilter:
    /// `ST_MakeEnvelope($min_lon, $min_lat, $max_lon, $max_lat, 4326)::geography && <col>`
    ///
    /// The `&&` operator lets Postgres use a GiST spatial index for a cheap
    /// first pass before expensive `ST_*` predicates. Combine with a shape
    /// predicate for best performance:
    ///
    /// ```ignore
    /// // Fast bbox prefilter, then exact intersection.
    /// Delivery::objects()
    ///     .filter(|f| f.area().bounded_by(37.0, -123.0, 38.0, -122.0))
    ///     .filter(|f| f.area().intersects(&zone))
    ///     .fetch_all(&mut ctx).await?
    /// ```
    ///
    /// Argument order matches the `GeoPoint` (lat, lon) convention:
    /// `min_lat`, `min_lon`, `max_lat`, `max_lon`. The emission swaps to
    /// Postgres's (x, y) = (lon, lat) order internally.
    ///
    /// All four coordinate values flow through `push_bind` — no string
    /// interpolation of user-supplied data.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn bounded_by(
        self,
        min_lat: f64,
        min_lon: f64,
        max_lat: f64,
        max_lon: f64,
    ) -> crate::expr::Expr<bool> {
        crate::expr::Expr::from_node(crate::expr::node::ExprNode::Spatial(
            crate::expr::spatial::SpatialExpr::BoundedBy {
                field_column: self.column(),
                min_lat,
                min_lon,
                max_lat,
                max_lon,
            },
        ))
    }
}

// Convex_hull spatial aggregate
//
// `ST_ConvexHull(ST_Collect(<col>::geometry))::geography` folds a per-group
// set of geographies into the smallest convex polygon enclosing them. Mirrors
// the shape of the existing `count_by_region` / `cluster_by_proximity` / etc.
// spatial aggregates from Phase 6.5 — sits on `FieldRef<M, G: GeographyValue>`
// so it is callable from the same field-closure context. The outer
// `::geography` cast on the emit side is required for the typed `Polygon`
// decode (see `SpatialExpr::ConvexHull` emit body for the rationale).

#[cfg(feature = "spatial")]
impl<M: crate::model::Model, G: crate::geo::GeographyValue> FieldRef<M, G> {
    /// `ST_ConvexHull(ST_Collect(<col>::geometry))::geography` — per-group
    /// convex-hull aggregate. Returns the smallest convex polygon that
    /// encloses every non-null geometry value in the group, cast back to
    /// `geography` so the typed `Polygon` decode in `fetch_all` accepts
    /// the column.
    ///
    /// # Why an aggregate
    ///
    /// PostGIS does not ship a one-shot convex-hull aggregate. The
    /// canonical pattern is `ST_ConvexHull(ST_Collect(...))` where
    /// `ST_Collect` is the actual aggregate (folding the per-row geometries
    /// into a single multi-geometry) and `ST_ConvexHull` is a scalar
    /// wrapper applied to the collected set. This method emits the fused
    /// form with an outer `::geography` cast; the typed surface presents
    /// it as a single [`AggregateExpr<Polygon>`].
    ///
    /// # Composition
    ///
    /// Use it inside `GroupedQuerySet::annotate(...)` like any other
    /// aggregate — typically alongside a per-group key from
    /// `QuerySet::group_by(|f| f.herd_id())`:
    ///
    /// ```ignore
    /// // Per-herd territory hulls — feeds the mating-pairs territory-overlap
    /// // scoring in the elephant-tracker demo.
    /// let hulls: Vec<(i64, Polygon)> = Elephant::objects()
    ///     .group_by(|f| f.herd_id())
    ///     .annotate(|f| f.location().convex_hull())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    ///
    /// # Return type
    ///
    /// Pinned to `AggregateExpr<Polygon>` because the typical multi-point
    /// input always yields a `Polygon`. Degenerate inputs (a single point,
    /// or two collinear points) yield `Point` / `LineString` respectively;
    /// callers feeding such inputs see a runtime EWKB-decode error and
    /// should bind a stricter input set or use `ctx.raw_scalar` with an
    /// untyped JSON / WKT decode path.
    ///
    /// # Where
    ///
    /// - [`crate::expr::spatial::SpatialExpr::ConvexHull`] — IR variant
    ///   that the typed surface stores inside the `AggregateExpr` envelope.
    /// - [`crate::query::QuerySet::group_by`] — produces the
    ///   `GroupedQuerySet` that consumes this aggregate via `.annotate(...)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn convex_hull(self) -> crate::expr::AggregateExpr<crate::geo::Polygon> {
        // The convex-hull SQL token stream lives in the SpatialExpr family,
        // not the ordinary AggOp set — `ST_ConvexHull(ST_Collect(<col>))`
        // is a fused two-call shape that doesn't fit the
        // `<KEYWORD>([DISTINCT] <expr>)` template `emit_unary_agg` uses.
        // Wrap it in `AggregateExpr` directly so the annotate plumbing
        // (which only inspects the `cast_to` / `window` slots on the
        // outer node) treats it as any other aggregate.
        //
        // Cluster E followup: migrate to AggOp::ConvexHull so the modifier
        // suite (.distinct() / .filter() / .over() / .order_by()) composes
        // automatically. Centroid + Collect (T12) ship through AggOp from
        // the start; convex_hull stays SpatialExpr-routed for now to limit
        // T12's blast radius.
        crate::expr::AggregateExpr::from_node(crate::expr::node::ExprNode::Spatial(
            crate::expr::spatial::SpatialExpr::ConvexHull {
                field_column: self.column(),
            },
        ))
    }

    /// `ST_Centroid(ST_Collect(<col>))::geography` — per-group centroid of
    /// the collected point geometries. Returns `AggregateExpr<GeoPoint>`.
    ///
    /// # SQL emission
    ///
    /// ```sql
    /// ST_Centroid(ST_Collect(<col>::geometry))::geography
    /// ```
    ///
    /// The `::geometry` inner cast matches the same cast discipline as
    /// the geometry-only shape predicates and `convex_hull` — `ST_Collect`
    /// requires `geometry` arguments and has no `geography` overload. The
    /// outer `::geography` cast keeps the round-trip on the geography
    /// substrate so the result decodes into `GeoPoint`.
    ///
    /// # Composition
    ///
    /// Used inside `GroupedQuerySet::annotate(...)` alongside other
    /// aggregates:
    ///
    /// ```ignore
    /// // Per-cluster centroid + count over a DBSCAN clustering — the
    /// // shape that backs the cluster_sightings demo's typed retrofit.
    /// let clusters: Vec<(ClusterId, GeoPoint, i64)> = Sighting::objects()
    ///     .cluster_by_proximity(
    ///         |f| f.location(),
    ///         ClusterRadius::meters(50_000.0).min_points(3),
    ///     )
    ///     .annotate(|f| (f.location().centroid(), f.id().count_star()))
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    ///
    /// # Postgres NULL behaviour
    ///
    /// Empty groups (or all-NULL inputs) produce SQL NULL; the typed
    /// surface decodes that as a runtime error on the non-`Option`
    /// surface. Wrap `Out = Option<GeoPoint>` at the call site if your
    /// dataset has known empty groups, or use a `FILTER (WHERE ...)`
    /// guard.
    ///
    /// # Why a typed aggregate, not raw SQL
    ///
    /// Adopters wanting a per-group centroid had to drop to
    /// `ctx.raw_rows("... ST_Centroid(ST_Collect(loc))::geography ...")`
    /// before this method — losing the typed annotate composition with
    /// `count_star`, `array_agg`, etc. The typed surface keeps the whole
    /// expression in the `GroupedQuerySet` chain.
    #[cfg(feature = "spatial")]
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn centroid(self) -> crate::expr::AggregateExpr<crate::geo::GeoPoint> {
        // Routes through the AggOp variant so all aggregate modifiers
        // (.distinct, .filter, .over, .order_by) compose automatically
        // via the AggregateExpr envelope. Special emission lives in
        // expr::sql::emit_expr's SpatialCentroid arm.
        crate::expr::AggregateExpr::unary_agg(
            crate::expr::node::AggOp::SpatialCentroid,
            self.column(),
            None,
        )
    }

    /// `ST_Collect(<col>)::geography` — per-group multi-point collection.
    /// Returns `AggregateExpr<MultiPoint>`.
    ///
    /// # SQL emission
    ///
    /// ```sql
    /// ST_Collect(<col>::geometry)::geography
    /// ```
    ///
    /// Same cast discipline as [`Self::centroid`] — inner `::geometry`
    /// for `ST_Collect`'s input, outer `::geography` for round-trip.
    ///
    /// # Composition
    ///
    /// Useful when a downstream Rust-side computation needs every
    /// contributing point of a group as a single multi-geometry value.
    /// For density-based clustering plus per-cluster point dump:
    ///
    /// ```ignore
    /// let clusters: Vec<(ClusterId, MultiPoint)> = Sighting::objects()
    ///     .cluster_by_proximity(
    ///         |f| f.location(),
    ///         ClusterRadius::meters(50_000.0).min_points(3),
    ///     )
    ///     .annotate(|f| f.location().collect())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    ///
    /// # Postgres NULL behaviour
    ///
    /// Empty groups produce SQL NULL — same caveat as [`Self::centroid`].
    #[cfg(feature = "spatial")]
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn collect(self) -> crate::expr::AggregateExpr<crate::geo::MultiPoint> {
        crate::expr::AggregateExpr::unary_agg(
            crate::expr::node::AggOp::SpatialCollect,
            self.column(),
            None,
        )
    }

    /// `ST_Extent(<col>::geometry)::geometry::geography` — per-group 2D
    /// bounding-box aggregate, returned as a four-vertex Polygon.
    ///
    /// # SQL emission
    ///
    /// ```sql
    /// ST_Extent(<col>::geometry)::geometry::geography
    /// ```
    ///
    /// `ST_Extent` returns Postgres' `box2d` type (a flat
    /// `(minx, miny, maxx, maxy)` quadruple) which has no direct
    /// `geography` cast. The two-step `::geometry::geography` cast chain
    /// projects the box into a four-vertex rectangular Polygon and moves
    /// it onto the geography substrate so the typed surface decodes
    /// into [`crate::geo::Polygon`].
    ///
    /// # Composition
    ///
    /// Use as the bounding-box per group:
    ///
    /// ```ignore
    /// let bboxes: Vec<(i64, Polygon)> = Sighting::objects()
    ///     .group_by(|f| f.cluster_id())
    ///     .annotate(|f| f.location().extent())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    ///
    /// # Postgres NULL behaviour
    ///
    /// Empty groups produce SQL NULL — wrap `Out = Option<Polygon>` at
    /// the call site for datasets with known empty groups.
    #[cfg(feature = "spatial")]
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn extent(self) -> crate::expr::AggregateExpr<crate::geo::Polygon> {
        crate::expr::AggregateExpr::unary_agg(
            crate::expr::node::AggOp::SpatialExtent,
            self.column(),
            None,
        )
    }

    /// `ST_3DExtent(<col>::geometry)::geometry::geography` — per-group
    /// 3D bounding-box aggregate, projected to its 2D footprint Polygon.
    ///
    /// # SQL emission
    ///
    /// ```sql
    /// ST_3DExtent(<col>::geometry)::geometry::geography
    /// ```
    ///
    /// Identical cast chain to [`Self::extent`] — `ST_3DExtent` returns
    /// `box3d`, neither of which casts directly to `geography`. The
    /// geometry-side cast projects the 3D box to a 2D rectangular
    /// Polygon (the footprint at the box's z-mid plane); the
    /// geography-side cast keeps the value on the geography substrate
    /// so the typed surface decodes into [`crate::geo::Polygon`].
    ///
    /// # Why a 2D return for a 3D aggregate
    ///
    /// Djogi's geography surface is 2D-only — `GeoPoint` has no
    /// elevation, [`crate::geo::Polygon`] has no Z dimension. Adopters
    /// with true 3D data should reach for `ctx.raw_scalar` against the
    /// `box3d` type directly. This typed surface gives the 2D footprint
    /// of a 3D bounding box, which is what most callers want when
    /// rendering on a 2D map.
    ///
    /// # Postgres NULL behaviour
    ///
    /// Empty groups produce SQL NULL — same caveat as [`Self::extent`].
    #[cfg(feature = "spatial")]
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn extent_3d(self) -> crate::expr::AggregateExpr<crate::geo::Polygon> {
        crate::expr::AggregateExpr::unary_agg(
            crate::expr::node::AggOp::SpatialExtent3D,
            self.column(),
            None,
        )
    }
}

// ── union() — region-merging aggregate, polygon-shaped fields only ──────────
//
// `ST_Union` produces a MultiPolygon for polygonal inputs; for point
// inputs it would yield a MultiPoint instead, breaking the typed
// `AggregateExpr<MultiPolygon>` decode. Restricting the receiver to
// `Polygon` / `MultiPolygon` fields keeps the typed surface sound;
// adopters wanting union semantics on points use the existing
// `collect()` (T12), which produces a MultiPoint.
#[cfg(feature = "spatial")]
impl<M: crate::model::Model> FieldRef<M, crate::geo::Polygon> {
    /// `ST_Union(<col>::geometry)::geography` — per-group region-merging
    /// aggregate. Folds a per-group set of polygons into a single
    /// MultiPolygon by merging shared edges.
    ///
    /// # SQL emission
    ///
    /// ```sql
    /// ST_Union(<col>::geometry)::geography
    /// ```
    ///
    /// Same cast discipline as the rest of the PostGIS aggregate
    /// family — inner `::geometry` for `ST_Union`'s argument, outer
    /// `::geography` for the typed-decode round-trip.
    ///
    /// # vs. [`Self::collect`] (when available) and [`Self::convex_hull`]
    ///
    /// - `ST_Union` *merges* overlapping/touching polygons, eliminating
    ///   shared edges — output area is the union of input areas.
    /// - `ST_Collect` (T12, available on points) builds a multi-geometry
    ///   without merging — output is the bag of inputs.
    /// - `ST_ConvexHull(ST_Collect(...))` returns the smallest convex
    ///   polygon enclosing all inputs — strictly larger than the union
    ///   for non-convex shapes.
    ///
    /// # Composition
    ///
    /// ```ignore
    /// // Merge per-region polygons into a single MultiPolygon per herd.
    /// let territories: Vec<(HerdId, MultiPolygon)> = Range::objects()
    ///     .group_by(|f| f.herd_id())
    ///     .annotate(|f| f.boundary().union())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    ///
    /// # Postgres NULL behaviour
    ///
    /// Empty groups produce SQL NULL — wrap `Out = Option<MultiPolygon>`
    /// at the call site for datasets with known empty groups.
    ///
    /// # Memory characteristics
    ///
    /// `ST_Union` sorts inputs and merges along a shared edge tree —
    /// efficient for moderate group sizes but memory-intensive for very
    /// large inputs. For terabyte-scale datasets, prefer the algorithm
    /// in `mem_union()` (a future Cluster E task) which uses pairwise
    /// merging with bounded working memory.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn union(self) -> crate::expr::AggregateExpr<crate::geo::MultiPolygon> {
        crate::expr::AggregateExpr::unary_agg(
            crate::expr::node::AggOp::SpatialUnion,
            self.column(),
            None,
        )
    }

    /// `ST_Collect(<col>::geometry)::geography` — per-group polygon
    /// aggregate that produces a single MultiPolygon. Portable
    /// fallback for `ST_PolygonAgg`.
    ///
    /// # SQL emission
    ///
    /// ```sql
    /// ST_Collect(<col>::geometry)::geography
    /// ```
    ///
    /// # Why a fallback emission
    ///
    /// `ST_PolygonAgg` is PostGIS 3.5+; Djogi's documented PostGIS
    /// floor is 3.x (see `docs/guide/spatial.md`). Emitting
    /// `ST_Collect` keeps the typed surface working on every Djogi-
    /// supported PostGIS version while producing an equivalent
    /// MultiPolygon for polygon-typed inputs. If Djogi ever raises
    /// the floor, only the emitter arm changes — the typed surface
    /// stays identical.
    ///
    /// # vs. [`Self::union`]
    ///
    /// - `polygon_agg()` collects polygons into a MultiPolygon
    ///   without merging — output is the bag of inputs.
    /// - `union()` merges overlapping/touching polygons — output is
    ///   the geometric union.
    ///
    /// # Composition
    ///
    /// ```ignore
    /// let regions: Vec<(HerdId, MultiPolygon)> = Range::objects()
    ///     .group_by(|f| f.herd_id())
    ///     .annotate(|f| f.boundary().polygon_agg())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    ///
    /// # Postgres NULL behaviour
    ///
    /// Empty groups produce SQL NULL.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn polygon_agg(self) -> crate::expr::AggregateExpr<crate::geo::MultiPolygon> {
        crate::expr::AggregateExpr::unary_agg(
            crate::expr::node::AggOp::SpatialPolygonAgg,
            self.column(),
            None,
        )
    }

    /// `ST_ClusterIntersecting(<col>::geometry)::geography[]` — per-
    /// group clustering aggregate that groups mutually-intersecting
    /// input polygons into per-cluster collections.
    ///
    /// # SQL emission
    ///
    /// ```sql
    /// ST_ClusterIntersecting(<col>::geometry)::geography[]
    /// ```
    ///
    /// # Return type
    ///
    /// PostGIS returns a `geometry[]` array — one element per cluster.
    /// The trailing `::geography[]` cast moves the array's element
    /// type onto the geography substrate so the typed surface decodes
    /// into `Vec<MultiPolygon>`. Each `MultiPolygon` element holds
    /// the polygons that mutually intersect; non-intersecting polygons
    /// land in their own single-element clusters.
    ///
    /// # Aggregate vs window-function clustering
    ///
    /// Unlike the existing `cluster_by_proximity` (a window function
    /// that adds a per-row cluster id), `cluster_intersecting()` is a
    /// true aggregate — one row out per (group, cluster) pair after
    /// the array is `unnest`ed at the call site. The aggregate form
    /// suits per-group folding (e.g. \"all overlapping ranges per
    /// herd\") whereas the window form suits per-row tagging.
    ///
    /// # Composition
    ///
    /// ```ignore
    /// // Per-herd intersecting territory clusters.
    /// let clusters: Vec<(HerdId, Vec<MultiPolygon>)> = Range::objects()
    ///     .group_by(|f| f.herd_id())
    ///     .annotate(|f| f.boundary().cluster_intersecting())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    ///
    /// # Postgres NULL behaviour
    ///
    /// Empty groups produce SQL NULL — wrap
    /// `Out = Option<Vec<MultiPolygon>>` for groups that may be empty.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn cluster_intersecting(self) -> crate::expr::AggregateExpr<Vec<crate::geo::MultiPolygon>> {
        crate::expr::AggregateExpr::unary_agg(
            crate::expr::node::AggOp::SpatialClusterIntersecting,
            self.column(),
            None,
        )
    }

    /// `ST_ClusterWithin(<col>::geometry, $1)::geography[]` — per-
    /// group clustering aggregate that groups input polygons within
    /// `distance` meters of each other.
    ///
    /// # SQL emission
    ///
    /// ```sql
    /// ST_ClusterWithin(<col>::geometry, $n)::geography[]
    /// ```
    ///
    /// `distance` is bound as a positional parameter — no string
    /// interpolation of user-supplied data.
    ///
    /// # vs. [`Self::cluster_intersecting`]
    ///
    /// - `cluster_intersecting()` clusters geometries that *touch
    ///   or overlap*.
    /// - `cluster_within(d)` clusters geometries within `d` meters
    ///   of each other — the threshold is configurable, so adopters
    ///   can tune cluster granularity per use case.
    ///
    /// # Composition
    ///
    /// ```ignore
    /// // Cluster nearby territories within 10 km.
    /// let clusters: Vec<(HerdId, Vec<MultiPolygon>)> = Range::objects()
    ///     .group_by(|f| f.herd_id())
    ///     .annotate(|f| f.boundary().cluster_within(10_000.0))
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    ///
    /// # Postgres NULL behaviour
    ///
    /// Empty groups produce SQL NULL — same caveat as
    /// [`Self::cluster_intersecting`].
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn cluster_within(
        self,
        distance: f64,
    ) -> crate::expr::AggregateExpr<Vec<crate::geo::MultiPolygon>> {
        crate::expr::AggregateExpr::unary_agg(
            crate::expr::node::AggOp::SpatialClusterWithin(distance),
            self.column(),
            None,
        )
    }

    /// `ST_MemUnion(<col>::geometry)::geography` — memory-friendly
    /// pairwise-merge variant of [`Self::union`].
    ///
    /// # SQL emission
    ///
    /// ```sql
    /// ST_MemUnion(<col>::geometry)::geography
    /// ```
    ///
    /// # vs. [`Self::union`]
    ///
    /// Both fold polygonal inputs into a single MultiPolygon by
    /// merging shared edges — same input / output shape, different
    /// algorithm:
    ///
    /// - `union()` (`ST_Union`) sorts inputs and merges along a
    ///   shared edge tree. Faster for moderate group sizes; memory-
    ///   intensive for very large input sets because the entire input
    ///   must fit in working memory.
    /// - `mem_union()` (`ST_MemUnion`) runs a pairwise merge with
    ///   bounded working memory. Slower per-row but handles
    ///   terabyte-scale inputs without spilling.
    ///
    /// Pick `mem_union()` when input size exceeds working memory; pick
    /// `union()` for the common case.
    ///
    /// # Composition
    ///
    /// ```ignore
    /// // Memory-friendly union over a very large input set.
    /// let merged: Vec<(HerdId, MultiPolygon)> = Range::objects()
    ///     .group_by(|f| f.herd_id())
    ///     .annotate(|f| f.boundary().mem_union())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    ///
    /// # Postgres NULL behaviour
    ///
    /// Empty groups produce SQL NULL — same caveat as [`Self::union`].
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn mem_union(self) -> crate::expr::AggregateExpr<crate::geo::MultiPolygon> {
        crate::expr::AggregateExpr::unary_agg(
            crate::expr::node::AggOp::SpatialMemUnion,
            self.column(),
            None,
        )
    }
}

#[cfg(feature = "spatial")]
impl<M: crate::model::Model> FieldRef<M, crate::geo::LineString> {
    /// `ST_Polygonize(<col>::geometry)::geography` — builds polygons
    /// from a per-group set of LineString segments.
    ///
    /// # SQL emission
    ///
    /// ```sql
    /// ST_Polygonize(<col>::geometry)::geography
    /// ```
    ///
    /// # Return type
    ///
    /// PostGIS returns a GeometryCollection at the geometry level;
    /// the trailing `::geography` cast keeps the value on the
    /// geography substrate for the typed `MultiPolygon` decode. Works
    /// for the typical line-segments-to-region case (e.g. assembling
    /// administrative-boundary polygons from per-edge LineString
    /// rows). Pathological inputs (LineStrings that don't form closed
    /// rings) yield a degenerate output that may fail the typed EWKB
    /// decode at the call site.
    ///
    /// # LineString-only receiver
    ///
    /// `polygonize()` only makes sense for input LineStrings — the
    /// algorithm walks edge endpoints to find closed rings. Other
    /// geography shapes (Point, Polygon, MultiPolygon) would produce
    /// undefined output. The receiver-type gate enforces this at the
    /// impl-block level.
    ///
    /// # Composition
    ///
    /// ```ignore
    /// // Per-region polygons assembled from per-edge LineString rows.
    /// let regions: Vec<(RegionId, MultiPolygon)> = Edge::objects()
    ///     .group_by(|f| f.region_id())
    ///     .annotate(|f| f.geometry().polygonize())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    ///
    /// # Postgres NULL behaviour
    ///
    /// Empty groups produce SQL NULL.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn polygonize(self) -> crate::expr::AggregateExpr<crate::geo::MultiPolygon> {
        crate::expr::AggregateExpr::unary_agg(
            crate::expr::node::AggOp::SpatialPolygonize,
            self.column(),
            None,
        )
    }

    /// `ST_LineAgg(<col>::geometry)::geography` — per-group
    /// `MultiLineString` builder. Collects per-row `LineString` values
    /// into a single `MultiLineString`.
    ///
    /// # SQL emission
    ///
    /// ```sql
    /// ST_LineAgg(<col>::geometry)::geography
    /// ```
    ///
    /// Inner `::geometry` cast feeds PostGIS's geometry-only
    /// `ST_LineAgg`; outer `::geography` cast moves the result back to
    /// the geography substrate so the typed `MultiLineString` decode
    /// works.
    ///
    /// # Sibling: `make_line()` vs `line_agg()`
    ///
    /// - [`FieldRef<M, GeoPoint>::make_line`] takes per-row **points**
    ///   and joins them into a single `LineString`. Use when each row
    ///   contributes one vertex.
    /// - This method takes per-row **LineStrings** and collects them
    ///   into a `MultiLineString`. Use when each row already carries a
    ///   path (GPS sub-tracks per device, route segments per leg, etc.)
    ///   and the per-group output should be the parallel multi-shape.
    ///
    /// # Composition
    ///
    /// ```ignore
    /// // Per-route MultiLineString of all logged sub-tracks
    /// let multi_tracks: Vec<(RouteId, MultiLineString)> = SubTrack::objects()
    ///     .group_by(|f| f.route_id())
    ///     .annotate(|f| f.path().line_agg())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    ///
    /// # Postgres NULL behaviour
    ///
    /// Empty groups produce SQL NULL — wrap `Out = Option<MultiLineString>`
    /// at the call site if your dataset has known empty groups.
    ///
    /// # PostGIS version
    ///
    /// `ST_LineAgg` is PostgreSQL 17+ / PostGIS 3.5+. Djogi targets
    /// PG 18 + PostGIS 3.5 so the canonical keyword is the safe choice.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn line_agg(self) -> crate::expr::AggregateExpr<crate::geo::MultiLineString> {
        crate::expr::AggregateExpr::unary_agg(
            crate::expr::node::AggOp::SpatialLineAgg,
            self.column(),
            None,
        )
    }
}

#[cfg(feature = "spatial")]
impl<M: crate::model::Model> FieldRef<M, crate::geo::MultiPolygon> {
    /// `ST_Union(<col>::geometry)::geography` — per-group region-merging
    /// aggregate over `MultiPolygon` inputs. See
    /// [`FieldRef::<M, Polygon>::union`] for full documentation; the
    /// behaviour is identical, only the input column shape differs.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn union(self) -> crate::expr::AggregateExpr<crate::geo::MultiPolygon> {
        crate::expr::AggregateExpr::unary_agg(
            crate::expr::node::AggOp::SpatialUnion,
            self.column(),
            None,
        )
    }

    /// `ST_MemUnion(<col>::geometry)::geography` — see
    /// [`FieldRef::<M, Polygon>::mem_union`] for full documentation;
    /// the behaviour is identical, only the input column shape differs.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn mem_union(self) -> crate::expr::AggregateExpr<crate::geo::MultiPolygon> {
        crate::expr::AggregateExpr::unary_agg(
            crate::expr::node::AggOp::SpatialMemUnion,
            self.column(),
            None,
        )
    }

    /// `ST_ClusterIntersecting(<col>::geometry)::geography[]` — see
    /// [`FieldRef::<M, Polygon>::cluster_intersecting`] for full
    /// documentation; the behaviour is identical, only the input
    /// column shape differs.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn cluster_intersecting(self) -> crate::expr::AggregateExpr<Vec<crate::geo::MultiPolygon>> {
        crate::expr::AggregateExpr::unary_agg(
            crate::expr::node::AggOp::SpatialClusterIntersecting,
            self.column(),
            None,
        )
    }

    /// `ST_ClusterWithin(<col>::geometry, $1)::geography[]` — see
    /// [`FieldRef::<M, Polygon>::cluster_within`] for full
    /// documentation; the behaviour is identical, only the input
    /// column shape differs.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn cluster_within(
        self,
        distance: f64,
    ) -> crate::expr::AggregateExpr<Vec<crate::geo::MultiPolygon>> {
        crate::expr::AggregateExpr::unary_agg(
            crate::expr::node::AggOp::SpatialClusterWithin(distance),
            self.column(),
            None,
        )
    }
}

#[cfg(feature = "spatial")]
impl<M: crate::model::Model> FieldRef<M, crate::geo::GeoPoint> {
    /// `ST_MakeLine(<col>::geometry)::geography` — per-group LineString
    /// builder. Connects per-row points into a single LineString in row
    /// order, or per-aggregate ORDER BY order when `.order_by(field)`
    /// is chained.
    ///
    /// # SQL emission
    ///
    /// ```sql
    /// ST_MakeLine(<col>::geometry)::geography
    /// ```
    ///
    /// # Order-sensitivity
    ///
    /// Unlike most aggregates where row order is incidental, the
    /// resulting LineString's *vertex sequence* directly reflects
    /// input row order. This aggregate naturally consumes T1's
    /// `.order_by(field)` modifier — the per-aggregate ORDER BY
    /// clause lands inside the `ST_MakeLine` parens to control vertex
    /// sequence at the aggregate level (not the result-set level).
    ///
    /// # Composition — GPS track example
    ///
    /// ```ignore
    /// // Per-trip GPS track ordered by timestamp.
    /// let tracks: Vec<(TripId, LineString)> = Sample::objects()
    ///     .group_by(|f| f.trip_id())
    ///     .annotate(|f| f.position().make_line().order_by(f.recorded_at()))
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    ///
    /// # Postgres NULL behaviour
    ///
    /// Empty groups (or all-NULL inputs) produce SQL NULL. PostGIS
    /// `ST_MakeLine` requires at least 2 input points; the typed
    /// surface decodes a too-short result as a runtime EWKB error.
    /// Wrap `Out = Option<LineString>` for groups that may be too
    /// small.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn make_line(self) -> crate::expr::AggregateExpr<crate::geo::LineString> {
        crate::expr::AggregateExpr::unary_agg(
            crate::expr::node::AggOp::SpatialMakeLine,
            self.column(),
            None,
        )
    }

    /// Emits `ST_Distance(<col>, ST_Point($lon, $lat)::geography)` — returns
    /// great-circle distance in meters from `<col>` to `center`.
    ///
    /// Composes with `.filter`, `.annotate`, and `.order_by`:
    ///
    /// ```ignore
    /// let center = GeoPoint::new(37.7749, -122.4194).unwrap();
    ///
    /// // Filter by distance threshold.
    /// Store::objects()
    ///     .filter(|f| f.location().distance_to(&center).lt(5000.0))
    ///     .fetch_all(&mut ctx).await?
    /// ```
    ///
    /// This wraps the existing `SpatialExpr::Distance` IR variant that was
    /// added in Phase 6 but previously only used by the `.order_by_distance`
    /// shortcut. T10 exposes it as a first-class expression method so it
    /// composes cleanly anywhere an `Expr<f64>` is accepted.
    ///
    /// Bind order: `$1 = center.lon`, `$2 = center.lat`.
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn distance_to(self, center: &crate::geo::GeoPoint) -> crate::expr::Expr<f64> {
        crate::expr::Expr::from_node(crate::expr::node::ExprNode::Spatial(
            crate::expr::spatial::SpatialExpr::Distance {
                field_column: self.column(),
                center: *center,
            },
        ))
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

// ── Phase 7-Zero-2 T8: forward traversal over an optional FK / O2O ──────
//
// `OptionalRelationRef<V>` is the return type of the macro-emitted accessor
// for a nullable relation field (`Option<ForeignKey<T>>` /
// `Option<OneToOneField<T>>`) on a visage-scoped `{Visage}Fields` struct.
// It keeps the nullability honest at the type level: the caller cannot
// reach into the peer's `Fields` without first opting in to the SQL
// IS-NOT-NULL guard through [`OptionalRelationRef::map_filter`].
//
// # Design rationale — why a wrapper type (not `Option<Fields>`)?
//
// Returning `Option<PeerFields>` would force the macro to embed a runtime
// branch in emitted code (`if self.path.is_some() { Some(PeerFields { … }) }
// else { None }`). But at filter-build time the FK column may be NULL
// on zero rows, some rows, or every row — the `Option` lens lives in the
// result set, not in the filter tree. The correct shape is "compose a
// condition as if the FK is set, but guard the whole thing with
// `author_id IS NOT NULL`". That's exactly what `map_filter` emits.
//
// The nullability marker also drives the boundary symbol inspection that
// later tasks (T9 / T10) will use to reject mixing a required-FK accessor
// with an optional-FK accessor under the same visage scope.

/// Traversal handle for an optional forward relation (`Option<ForeignKey<T>>` or
/// `Option<OneToOneField<T>>`) from a visage-scoped `{Visage}Fields`.
///
/// # Why `OptionalRelationRef<V>` over `Option<V>`?
///
/// The nullability lives in the row shape, not in the filter tree. A
/// filter closure may compose a condition that "would" apply if the FK
/// is set — `OptionalRelationRef::map_filter` lifts that closure into a
/// `Condition` that first asserts the FK is non-NULL, then AND-s the
/// inner closure's output. The caller never sees `None` — the SQL guard
/// is automatic.
///
/// # SQL shape
///
/// `map_filter(|a| a.name().eq("Ada"))` on a wrapper over the `author_id`
/// FK emits:
///
/// ```sql
/// author_id IS NOT NULL AND author.name = $1
/// ```
///
/// The `author_id IS NOT NULL` clause keeps SQL three-valued logic
/// aligned with Rust's `Option` semantics: rows where the FK is NULL
/// are excluded from the match set, matching the user-level mental model
/// of "filter over author when author is set".
///
/// # Use from macro-emitted code only
///
/// Constructed through [`__macro_support::__make_optional_relation_ref`].
/// The field `fk_column` is the owning side's FK column (e.g.
/// `"author_id"`), and `peer_fields` is the path-threaded peer `Fields`
/// handle returned by the macro's traversal accessor.
pub struct OptionalRelationRef<V> {
    fk_column: &'static str,
    peer_fields: V,
}

impl<V> OptionalRelationRef<V> {
    /// Compose a predicate that applies to the peer only when the FK is
    /// non-NULL.
    ///
    /// The `f` closure receives the peer `Fields` handle by value (it's
    /// `Copy` on the macro-emitted shape) and must return a `Condition`.
    /// The returned `Condition` is equivalent to
    /// `Condition::and(fk IS NOT NULL, f(peer_fields))`.
    ///
    /// # Consuming `self`
    ///
    /// `map_filter` consumes the wrapper by value. `V: Clone` lets the
    /// closure receive an owned handle without forcing the wrapper's
    /// owner to pre-clone. For the `{Visage}Fields` case the peer handle
    /// is a plain ZST-shaped struct with a `&'static str` path, so
    /// `Clone` is trivially cheap.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn map_filter<F>(self, f: F) -> Condition
    where
        F: FnOnce(V) -> Condition,
    {
        let inner = f(self.peer_fields);
        let not_null = Condition::Leaf(Leaf::new(
            self.fk_column,
            LookupOp::IsNotNull,
            FilterValue::Null,
        ));
        Condition::and(not_null, inner)
    }

    /// Emit a standalone `fk_column IS NULL` predicate. Use from a
    /// closure when the caller wants to flip the guard — "match rows
    /// where the FK is absent".
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn is_none(self) -> Condition {
        Condition::Leaf(Leaf::new(
            self.fk_column,
            LookupOp::IsNull,
            FilterValue::Null,
        ))
    }

    /// Emit a standalone `fk_column IS NOT NULL` predicate. The
    /// complement of [`is_none`](Self::is_none).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn is_some(self) -> Condition {
        Condition::Leaf(Leaf::new(
            self.fk_column,
            LookupOp::IsNotNull,
            FilterValue::Null,
        ))
    }
}

impl<V: Copy> Copy for OptionalRelationRef<V> {}
impl<V: Clone> Clone for OptionalRelationRef<V> {
    fn clone(&self) -> Self {
        Self {
            fk_column: self.fk_column,
            peer_fields: self.peer_fields.clone(),
        }
    }
}

impl<V: std::fmt::Debug> std::fmt::Debug for OptionalRelationRef<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OptionalRelationRef")
            .field("fk_column", &self.fk_column)
            .field("peer_fields", &self.peer_fields)
            .finish()
    }
}

#[doc(hidden)]
pub mod optional_relation_support {
    //! Sealed constructor for [`OptionalRelationRef`]. Only
    //! macro-emitted code reaches in here; downstream callers are
    //! blocked by the `#[doc(hidden)]` marker and the
    //! double-underscore prefix on [`__make_optional_relation_ref`].
    use super::OptionalRelationRef;
    use crate::ident::assert_plain_ident;

    /// Construct an [`OptionalRelationRef<V>`]. The macro emits
    /// `__make_optional_relation_ref("author_id", UserPublicFields::with_path("author"))`
    /// for a `#[field(expose(public -> UserPublic))]` on
    /// `author: Option<ForeignKey<User>>`.
    ///
    /// `fk_column` is validated against [`assert_plain_ident`] before
    /// storage; the `peer_fields` is passed through by value (the
    /// macro already routed the peer's path through the shared
    /// identifier validator in
    /// `FieldRef::__macro_support::__make_field_ref_with_path`).
    #[doc(hidden)]
    pub fn __make_optional_relation_ref<V>(
        fk_column: &'static str,
        peer_fields: V,
    ) -> OptionalRelationRef<V> {
        assert_plain_ident(fk_column, "optional_fk_column");
        OptionalRelationRef {
            fk_column,
            peer_fields,
        }
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

// ── T10: bounded_by + distance_to method dispatch tests ──────────────────

#[cfg(all(test, feature = "spatial"))]
mod bbox_tests {
    use super::*;
    use crate::expr::node::ExprNode;
    use crate::expr::spatial::SpatialExpr;
    use crate::geo::{GeoPoint, MultiPolygon, Polygon};
    use crate::pg::accumulator::SqlAccumulator;
    use std::future::Future;

    // Minimal `Model` stub shared across T10 tests.
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

    // Helper: unwrap `Expr<bool>` -> `SpatialExpr` for assertion.
    fn unwrap_spatial_from_expr(expr: crate::expr::Expr<bool>) -> SpatialExpr {
        if let ExprNode::Spatial(s) = expr.node {
            return s;
        }
        panic!("expected ExprNode::Spatial(...)");
    }

    // Helper: minimal closed Polygon.
    fn make_polygon() -> Polygon {
        let ring = [
            GeoPoint::new(0.0, 0.0).unwrap(),
            GeoPoint::new(1.0, 0.0).unwrap(),
            GeoPoint::new(1.0, 1.0).unwrap(),
            GeoPoint::new(0.0, 1.0).unwrap(),
            GeoPoint::new(0.0, 0.0).unwrap(),
        ];
        Polygon::closed(&ring).unwrap()
    }

    /// `.bounded_by` on `FieldRef<Fake, GeoPoint>` must emit
    /// `ST_MakeEnvelope($1, $2, $3, $4, 4326)::geography && <col>`
    /// with bind order: $1=min_lon, $2=min_lat, $3=max_lon, $4=max_lat.
    #[test]
    fn bounded_by_emits_st_makeenvelope_in_xy_order() {
        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        // min_lat=37.0, min_lon=-123.0, max_lat=38.0, max_lon=-122.0
        let expr = field.bounded_by(37.0, -123.0, 38.0, -122.0);
        let s = unwrap_spatial_from_expr(expr);
        let mut acc = SqlAccumulator::new("");
        s.emit(&mut acc);
        let sql = acc.sql();
        assert!(
            sql.contains("ST_MakeEnvelope("),
            "expected ST_MakeEnvelope; got: {sql}"
        );
        assert!(
            sql.contains("::geography &&"),
            "expected ::geography &&; got: {sql}"
        );
        assert!(
            sql.contains("location"),
            "expected column 'location' after &&; got: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            4,
            "expected 4 binds; got {}",
            acc.bind_count()
        );
    }

    /// Coordinate values must not appear as literal text in the emitted SQL.
    #[test]
    fn bounded_by_emits_all_four_coords_as_binds() {
        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("loc");
        let expr = field.bounded_by(55.5555, 66.6666, 77.7777, 88.8888);
        let s = unwrap_spatial_from_expr(expr);
        let mut acc = SqlAccumulator::new("");
        s.emit(&mut acc);
        let sql = acc.sql();
        assert!(!sql.contains("55.5555"), "min_lat leaked; got: {sql}");
        assert!(!sql.contains("66.6666"), "min_lon leaked; got: {sql}");
        assert!(!sql.contains("77.7777"), "max_lat leaked; got: {sql}");
        assert!(!sql.contains("88.8888"), "max_lon leaked; got: {sql}");
        assert_eq!(acc.bind_count(), 4);
    }

    /// `.bounded_by` is generic over `GeographyValue` — verify it compiles
    /// and produces a `BoundedBy` node for `Polygon` and `MultiPolygon`
    /// fields (not just `GeoPoint`).
    #[test]
    fn bounded_by_works_on_polygon_and_multipolygon_fieldrefs() {
        // Polygon field
        let poly_field: FieldRef<Fake, Polygon> = FieldRef::new("area");
        let expr_poly = poly_field.bounded_by(37.0, -123.0, 38.0, -122.0);
        let s_poly = unwrap_spatial_from_expr(expr_poly);
        assert!(
            matches!(
                s_poly,
                SpatialExpr::BoundedBy {
                    field_column: "area",
                    ..
                }
            ),
            "expected BoundedBy on Polygon field; got {s_poly:?}"
        );

        // MultiPolygon field
        let mpoly = make_polygon();
        let mp_field: FieldRef<Fake, MultiPolygon> = FieldRef::new("coverage");
        let _ = mpoly; // make_polygon is just to satisfy type-checker in make fn; field is enough
        let expr_mp = mp_field.bounded_by(0.0, 0.0, 1.0, 1.0);
        let s_mp = unwrap_spatial_from_expr(expr_mp);
        assert!(
            matches!(
                s_mp,
                SpatialExpr::BoundedBy {
                    field_column: "coverage",
                    ..
                }
            ),
            "expected BoundedBy on MultiPolygon field; got {s_mp:?}"
        );
    }
}

#[cfg(all(test, feature = "spatial"))]
mod distance_tests {
    use super::*;
    use crate::expr::node::ExprNode;
    use crate::expr::spatial::SpatialExpr;
    use crate::geo::{GeoPoint, Polygon};
    use crate::pg::accumulator::SqlAccumulator;
    use std::future::Future;

    // Minimal closed Polygon — used by the Cluster C T17 typed-surface tests
    // below. Mirrors the helper in `bbox_tests`; duplicated locally because
    // each `#[cfg(test)] mod` is its own item scope.
    fn make_polygon() -> Polygon {
        let ring = [
            GeoPoint::new(0.0, 0.0).unwrap(),
            GeoPoint::new(1.0, 0.0).unwrap(),
            GeoPoint::new(1.0, 1.0).unwrap(),
            GeoPoint::new(0.0, 1.0).unwrap(),
            GeoPoint::new(0.0, 0.0).unwrap(),
        ];
        Polygon::closed(&ring).unwrap()
    }

    // Minimal `Model` stub for distance_to tests.
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

    /// `.distance_to` must emit `ST_Distance(<col>, ST_Point($lon, $lat)::geography)`.
    #[test]
    fn distance_to_emits_st_distance() {
        let center = GeoPoint::new(37.7749, -122.4194).unwrap();
        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("loc");
        let expr: crate::expr::Expr<f64> = field.distance_to(&center);
        if let ExprNode::Spatial(SpatialExpr::Distance {
            field_column,
            center: c,
        }) = expr.node
        {
            assert_eq!(field_column, "loc");
            // Verify the center was stored correctly.
            assert!((c.lat - 37.7749).abs() < 1e-10, "lat mismatch: {}", c.lat);
            assert!(
                (c.lon - (-122.4194)).abs() < 1e-10,
                "lon mismatch: {}",
                c.lon
            );
            // Emit and check SQL structure.
            let mut acc = SqlAccumulator::new("");
            SpatialExpr::Distance {
                field_column,
                center: c,
            }
            .emit(&mut acc);
            let sql = acc.sql();
            assert!(
                sql.contains("ST_Distance"),
                "expected ST_Distance; got: {sql}"
            );
            assert!(sql.contains("loc"), "expected column; got: {sql}");
            assert!(
                sql.contains("::geography"),
                "expected ::geography; got: {sql}"
            );
            assert_eq!(acc.bind_count(), 2, "expected 2 binds (lon, lat)");
        } else {
            panic!("expected Distance spatial expr");
        }
    }

    /// `.distance_to(&center).lt(1000.0)` must produce `Expr<bool>` wrapping
    /// a `Cmp` node whose LHS is a `Spatial(Distance(...))` node.
    #[test]
    fn distance_to_composes_with_filter_lt() {
        let center = GeoPoint::new(48.8566, 2.3522).unwrap();
        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("position");
        // `.lt(1000.0f64)` is on `Expr<f64>` — requires `f64: Into<Expr<f64>>`.
        let predicate: crate::expr::Expr<bool> = field.distance_to(&center).lt(1000.0f64);
        // The result should be a Cmp node whose LHS is a Distance spatial expr.
        if let ExprNode::Cmp { lhs, .. } = predicate.node
            && let ExprNode::Spatial(SpatialExpr::Distance { field_column, .. }) = *lhs
        {
            assert_eq!(field_column, "position");
            return;
        }
        panic!("expected Cmp {{ lhs: Spatial(Distance {{..}}) }}");
    }

    /// `.distance_to` must return `Expr<f64>` — verified by type inference
    /// (the binding annotation below is the check; if the return type were
    /// wrong this would not compile).
    #[test]
    fn distance_to_produces_f64_expr() {
        let center = GeoPoint::new(0.0, 0.0).unwrap();
        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("loc");
        // Type annotation is the compile-time assertion.
        let _expr: crate::expr::Expr<f64> = field.distance_to(&center);
    }

    // Convex_hull typed surface tests

    /// `FieldRef<M, GeoPoint>::convex_hull()` must produce an
    /// `AggregateExpr<Polygon>` whose underlying node is the
    /// `SpatialExpr::ConvexHull { field_column }` IR variant. The typed
    /// return is a compile-time assertion; the runtime check pins the
    /// stored field column.
    #[cfg(feature = "spatial")]
    #[test]
    fn convex_hull_on_geopoint_field_produces_aggregate_polygon() {
        use crate::expr::AggregateExpr;
        use crate::geo::Polygon as PolygonTy;

        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        // Compile-time check on the return type.
        let agg: AggregateExpr<PolygonTy> = field.convex_hull();
        // Runtime check — the IR node carries the column name.
        if let ExprNode::Spatial(SpatialExpr::ConvexHull { field_column }) = agg.node {
            assert_eq!(field_column, "location");
            return;
        }
        panic!("expected ExprNode::Spatial(SpatialExpr::ConvexHull {{..}})");
    }

    /// `convex_hull` is generic over `GeographyValue` — verify it dispatches
    /// from a `Polygon` field as well as a `GeoPoint` field. The mating-pairs
    /// demo uses it on point columns; spec keeps the generic surface so
    /// callers with polygonal range columns can fold those into a hull too.
    #[cfg(feature = "spatial")]
    #[test]
    fn convex_hull_dispatches_from_polygon_field_too() {
        use crate::expr::AggregateExpr;
        use crate::geo::Polygon as PolygonTy;

        let field: FieldRef<Fake, PolygonTy> = FieldRef::new("territory");
        let agg: AggregateExpr<PolygonTy> = field.convex_hull();
        if let ExprNode::Spatial(SpatialExpr::ConvexHull { field_column }) = agg.node {
            assert_eq!(field_column, "territory");
            return;
        }
        panic!("expected ConvexHull on Polygon field");
    }

    // ── centroid / collect — T12 PostGIS aggregates ──────────────────────────

    #[cfg(feature = "spatial")]
    #[test]
    fn centroid_on_geopoint_field_produces_aggregate_geopoint() {
        use crate::expr::AggregateExpr;
        use crate::expr::node::AggOp;

        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        // Compile-time check on the return type.
        let agg: AggregateExpr<GeoPoint> = field.centroid();
        // Runtime check — the IR node is the AggOp variant.
        if let ExprNode::Aggregate { op, arg, .. } = agg.node {
            assert!(matches!(op, AggOp::SpatialCentroid));
            if let ExprNode::Field { column } = *arg {
                assert_eq!(column, "location");
                return;
            }
            panic!("expected Aggregate.arg to wrap the column");
        }
        panic!("expected ExprNode::Aggregate(SpatialCentroid)");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn collect_on_geopoint_field_produces_aggregate_multipoint() {
        use crate::expr::AggregateExpr;
        use crate::expr::node::AggOp;
        use crate::geo::MultiPoint;

        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let agg: AggregateExpr<MultiPoint> = field.collect();
        if let ExprNode::Aggregate { op, arg, .. } = agg.node {
            assert!(matches!(op, AggOp::SpatialCollect));
            if let ExprNode::Field { column } = *arg {
                assert_eq!(column, "location");
                return;
            }
            panic!("expected Aggregate.arg to wrap the column");
        }
        panic!("expected ExprNode::Aggregate(SpatialCollect)");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn centroid_emits_st_centroid_st_collect_with_geography_cast() {
        use crate::pg::accumulator::SqlAccumulator;
        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let agg = field.centroid();
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        assert_eq!(
            acc.sql(),
            "ST_Centroid(ST_Collect(location::geometry))::geography"
        );
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn collect_emits_st_collect_with_geography_cast() {
        use crate::pg::accumulator::SqlAccumulator;
        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let agg = field.collect();
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "ST_Collect(location::geometry)::geography");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn centroid_with_distinct_emits_distinct_inside_st_collect() {
        // DISTINCT lands inside ST_Collect — the actual aggregating step.
        // ST_Centroid is a post-aggregate scalar wrapper.
        use crate::pg::accumulator::SqlAccumulator;
        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let agg = field.centroid().distinct();
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        assert_eq!(
            acc.sql(),
            "ST_Centroid(ST_Collect(DISTINCT location::geometry))::geography"
        );
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn centroid_with_filter_attaches_to_inner_st_collect() {
        // Codex T22 BLOCK-1: FILTER (WHERE ...) must attach to the
        // inner ST_Collect aggregate, BEFORE the outer ST_Centroid
        // wrapper and the ::geography cast. Postgres rejects FILTER
        // after a cast.
        //
        // Correct shape:
        //   ST_Centroid(ST_Collect(<col>::geometry) FILTER (WHERE <cond>))::geography
        //
        // ST_Centroid is a scalar wrapper, not an aggregate; FILTER
        // attaches to ST_Collect (the actual aggregate).
        use crate::expr::Expr;
        use crate::pg::accumulator::SqlAccumulator;
        let loc: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let confidence: FieldRef<Fake, f64> = FieldRef::new("confidence");
        let agg = loc
            .centroid()
            .filter(confidence.as_expr().gt(Expr::literal(0.5_f64)));
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        let sql = acc.sql().to_string();
        assert!(
            sql.starts_with("ST_Centroid(ST_Collect(location::geometry)"),
            "must start with ST_Centroid(ST_Collect(...), got: {sql}"
        );
        assert!(
            sql.contains(" FILTER (WHERE confidence > "),
            "FILTER clause must be present, got: {sql}"
        );
        assert!(
            sql.ends_with(")::geography"),
            "must end with )::geography (cast outside ST_Centroid), got: {sql}"
        );
        // Critical invariant: ::geography appears AFTER the FILTER
        // clause closes, not before. Verify FILTER index < geography
        // index.
        let filter_idx = sql.find(" FILTER (WHERE").unwrap();
        let cast_idx = sql.rfind("::geography").unwrap();
        assert!(
            filter_idx < cast_idx,
            "FILTER must precede ::geography cast for valid Postgres syntax; got: {sql}"
        );
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn centroid_with_over_in_annotate_path_places_over_inside_geography_cast() {
        // Codex T22 round-3 BLOCK-1: when a spatial aggregate is
        // emitted through the windowed-annotate path (the default
        // `OVER ()` for ungrouped annotate, or an explicit
        // `.over(|w| ...)` window spec), the OVER clause must land
        // inside the outer `::geography` cast — Postgres's
        // aggregate-call grammar places `OVER` on the bare aggregate
        // before any post-call scalar wrapper.
        //
        // Correct shape:
        //   (ST_Centroid(ST_Collect(<col>::geometry)) OVER (...))::geography
        //
        // Wrong shape (pre-fix):
        //   ST_Centroid(ST_Collect(<col>::geometry))::geography OVER (...)
        //
        // The cast attaches to OVER's result rather than the
        // aggregate, which Postgres rejects as a syntax error.
        use crate::pg::accumulator::SqlAccumulator;
        let loc: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let agg = loc.centroid();
        let mut acc = SqlAccumulator::new("");
        crate::query::sql::emit_aggregate_with_window_and_cast(&mut acc, &agg.node);
        let sql = acc.sql().to_string();
        assert_eq!(
            sql, "(ST_Centroid(ST_Collect(location::geometry)) OVER ())::geography",
            "OVER must fall inside the ::geography cast; got: {sql}"
        );
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn collect_with_over_in_annotate_path_places_over_inside_geography_cast() {
        // Same invariant as centroid — `loc.collect()` going through
        // the windowed path must produce
        // `(ST_Collect(...) OVER ())::geography`, not
        // `ST_Collect(...)::geography OVER ()`.
        use crate::pg::accumulator::SqlAccumulator;
        let loc: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let agg = loc.collect();
        let mut acc = SqlAccumulator::new("");
        crate::query::sql::emit_aggregate_with_window_and_cast(&mut acc, &agg.node);
        let sql = acc.sql().to_string();
        assert_eq!(
            sql, "(ST_Collect(location::geometry) OVER ())::geography",
            "OVER must fall inside the ::geography cast; got: {sql}"
        );
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn centroid_with_filter_and_over_places_both_inside_geography_cast() {
        // Combined FILTER + OVER under spatial cast — the most
        // structurally demanding case. Both modifiers must fall
        // inside the cast, in canonical Postgres order:
        //   (AGG(...) FILTER (WHERE ...) OVER (...))::geography
        use crate::expr::Expr;
        use crate::pg::accumulator::SqlAccumulator;
        let loc: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let confidence: FieldRef<Fake, f64> = FieldRef::new("confidence");
        let agg = loc
            .centroid()
            .filter(confidence.as_expr().gt(Expr::literal(0.5_f64)));
        let mut acc = SqlAccumulator::new("");
        crate::query::sql::emit_aggregate_with_window_and_cast(&mut acc, &agg.node);
        let sql = acc.sql().to_string();
        assert!(
            sql.starts_with("(ST_Centroid("),
            "must open with outer paren before ST_Centroid; got: {sql}"
        );
        assert!(
            sql.contains(" FILTER (WHERE confidence > "),
            "FILTER clause must be present; got: {sql}"
        );
        assert!(
            sql.contains(" OVER ()"),
            "OVER clause must be present; got: {sql}"
        );
        assert!(
            sql.ends_with(")::geography"),
            "must end with )::geography (cast outside paren-wrapped body); got: {sql}"
        );
        // Check ordering: FILTER < OVER < cast.
        let filter_idx = sql.find(" FILTER (").unwrap();
        let over_idx = sql.find(" OVER (").unwrap();
        let cast_idx = sql.rfind("::geography").unwrap();
        assert!(
            filter_idx < over_idx && over_idx < cast_idx,
            "modifier order must be FILTER < OVER < ::geography; got: {sql}"
        );
    }

    // ── T13 — union / extent / extent_3d ─────────────────────────────────────

    #[cfg(feature = "spatial")]
    #[test]
    fn union_on_polygon_field_produces_aggregate_multipolygon() {
        use crate::expr::AggregateExpr;
        use crate::expr::node::AggOp;
        use crate::geo::{MultiPolygon, Polygon as PolygonTy};

        let field: FieldRef<Fake, PolygonTy> = FieldRef::new("territory");
        let agg: AggregateExpr<MultiPolygon> = field.union();
        if let ExprNode::Aggregate { op, arg, .. } = agg.node {
            assert!(matches!(op, AggOp::SpatialUnion));
            if let ExprNode::Field { column } = *arg {
                assert_eq!(column, "territory");
                return;
            }
            panic!("expected Aggregate.arg to wrap the column");
        }
        panic!("expected ExprNode::Aggregate(SpatialUnion)");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn union_on_multipolygon_field_also_dispatches() {
        use crate::expr::AggregateExpr;
        use crate::geo::MultiPolygon;
        let field: FieldRef<Fake, MultiPolygon> = FieldRef::new("region");
        // Compile-time return-type check from the MultiPolygon-receiver impl.
        let _agg: AggregateExpr<MultiPolygon> = field.union();
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn union_emits_st_union_with_geography_cast() {
        use crate::geo::Polygon as PolygonTy;
        use crate::pg::accumulator::SqlAccumulator;
        let field: FieldRef<Fake, PolygonTy> = FieldRef::new("territory");
        let agg = field.union();
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "ST_Union(territory::geometry)::geography");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn union_with_distinct_emits_distinct_inside_st_union() {
        use crate::geo::Polygon as PolygonTy;
        use crate::pg::accumulator::SqlAccumulator;
        let field: FieldRef<Fake, PolygonTy> = FieldRef::new("territory");
        let agg = field.union().distinct();
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        assert_eq!(
            acc.sql(),
            "ST_Union(DISTINCT territory::geometry)::geography"
        );
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn extent_on_geopoint_field_produces_aggregate_polygon() {
        use crate::expr::AggregateExpr;
        use crate::expr::node::AggOp;
        use crate::geo::Polygon as PolygonTy;

        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let agg: AggregateExpr<PolygonTy> = field.extent();
        if let ExprNode::Aggregate { op, arg, .. } = agg.node {
            assert!(matches!(op, AggOp::SpatialExtent));
            if let ExprNode::Field { column } = *arg {
                assert_eq!(column, "location");
                return;
            }
            panic!("expected Aggregate.arg to wrap the column");
        }
        panic!("expected ExprNode::Aggregate(SpatialExtent)");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn extent_emits_box2d_geometry_geography_cast_chain() {
        use crate::pg::accumulator::SqlAccumulator;
        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let agg = field.extent();
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        // box2d → geometry → geography cast chain — ST_Extent has no
        // direct geography cast, the two-step cast keeps the typed
        // surface decoding into Polygon.
        assert_eq!(
            acc.sql(),
            "ST_Extent(location::geometry)::geometry::geography"
        );
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn extent_3d_on_geopoint_field_produces_aggregate_polygon() {
        use crate::expr::AggregateExpr;
        use crate::expr::node::AggOp;
        use crate::geo::Polygon as PolygonTy;

        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let agg: AggregateExpr<PolygonTy> = field.extent_3d();
        if let ExprNode::Aggregate { op, .. } = agg.node {
            assert!(matches!(op, AggOp::SpatialExtent3D));
            return;
        }
        panic!("expected ExprNode::Aggregate(SpatialExtent3D)");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn extent_3d_emits_st_3dextent_with_two_step_cast_chain() {
        use crate::pg::accumulator::SqlAccumulator;
        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let agg = field.extent_3d();
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        assert_eq!(
            acc.sql(),
            "ST_3DExtent(location::geometry)::geometry::geography"
        );
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn extent_with_filter_attaches_to_inner_aggregate_before_cast() {
        // Codex T22 BLOCK-1: FILTER must precede the cast chain.
        // Correct shape:
        //   (ST_Extent(<col>::geometry) FILTER (WHERE <cond>))::geometry::geography
        use crate::expr::Expr;
        use crate::pg::accumulator::SqlAccumulator;
        let loc: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let confidence: FieldRef<Fake, f64> = FieldRef::new("confidence");
        let agg = loc
            .extent()
            .filter(confidence.as_expr().gt(Expr::literal(0.5_f64)));
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        let sql = acc.sql().to_string();
        assert!(
            sql.starts_with("(ST_Extent(location::geometry)"),
            "must start with (ST_Extent for FILTER attachment, got: {sql}"
        );
        assert!(
            sql.contains(" FILTER (WHERE confidence > "),
            "FILTER clause must be present, got: {sql}"
        );
        assert!(
            sql.ends_with(")::geometry::geography"),
            "cast chain must close after FILTER, got: {sql}"
        );
        let filter_idx = sql.find(" FILTER (WHERE").unwrap();
        let cast_idx = sql.rfind("::geometry::geography").unwrap();
        assert!(
            filter_idx < cast_idx,
            "FILTER must precede the ::geometry::geography cast chain; got: {sql}"
        );
    }

    // ── T14 — make_line / polygon_agg ────────────────────────────────────────

    #[cfg(feature = "spatial")]
    #[test]
    fn make_line_on_geopoint_field_produces_aggregate_linestring() {
        use crate::expr::AggregateExpr;
        use crate::expr::node::AggOp;
        use crate::geo::LineString;

        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("position");
        let agg: AggregateExpr<LineString> = field.make_line();
        if let ExprNode::Aggregate { op, arg, .. } = agg.node {
            assert!(matches!(op, AggOp::SpatialMakeLine));
            if let ExprNode::Field { column } = *arg {
                assert_eq!(column, "position");
                return;
            }
            panic!("expected Aggregate.arg to wrap the column");
        }
        panic!("expected ExprNode::Aggregate(SpatialMakeLine)");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn make_line_emits_st_makeline_with_geography_cast() {
        use crate::pg::accumulator::SqlAccumulator;
        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("position");
        let agg = field.make_line();
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "ST_MakeLine(position::geometry)::geography");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn make_line_with_order_by_lands_inside_st_makeline_parens() {
        // Order-sensitive: `.order_by(other)` controls the LineString's
        // vertex sequence — the per-aggregate ORDER BY clause must land
        // inside ST_MakeLine's parens (before the closing paren and
        // before the geography cast), not after the whole expression.
        use crate::pg::accumulator::SqlAccumulator;
        let position: FieldRef<Fake, GeoPoint> = FieldRef::new("position");
        let recorded_at: FieldRef<Fake, i64> = FieldRef::new("recorded_at");
        let agg = position.make_line().order_by(recorded_at.asc());
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        assert_eq!(
            acc.sql(),
            "ST_MakeLine(position::geometry ORDER BY recorded_at ASC)::geography"
        );
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn polygon_agg_on_polygon_field_produces_aggregate_multipolygon() {
        use crate::expr::AggregateExpr;
        use crate::expr::node::AggOp;
        use crate::geo::{MultiPolygon, Polygon as PolygonTy};

        let field: FieldRef<Fake, PolygonTy> = FieldRef::new("territory");
        let agg: AggregateExpr<MultiPolygon> = field.polygon_agg();
        if let ExprNode::Aggregate { op, arg, .. } = agg.node {
            assert!(matches!(op, AggOp::SpatialPolygonAgg));
            if let ExprNode::Field { column } = *arg {
                assert_eq!(column, "territory");
                return;
            }
            panic!("expected Aggregate.arg to wrap the column");
        }
        panic!("expected ExprNode::Aggregate(SpatialPolygonAgg)");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn polygon_agg_emits_st_collect_portable_fallback() {
        // Portable fallback for ST_PolygonAgg (PostGIS 3.5+) — Djogi's
        // PostGIS floor is 3.x, so the emitter uses ST_Collect which
        // produces an equivalent MultiPolygon for polygon-typed inputs.
        use crate::geo::Polygon as PolygonTy;
        use crate::pg::accumulator::SqlAccumulator;
        let field: FieldRef<Fake, PolygonTy> = FieldRef::new("territory");
        let agg = field.polygon_agg();
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "ST_Collect(territory::geometry)::geography");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn polygon_agg_with_distinct_emits_distinct_inside_st_collect() {
        use crate::geo::Polygon as PolygonTy;
        use crate::pg::accumulator::SqlAccumulator;
        let field: FieldRef<Fake, PolygonTy> = FieldRef::new("territory");
        let agg = field.polygon_agg().distinct();
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        assert_eq!(
            acc.sql(),
            "ST_Collect(DISTINCT territory::geometry)::geography"
        );
    }

    // ── T15 — cluster_intersecting / cluster_within ──────────────────────────

    #[cfg(feature = "spatial")]
    #[test]
    fn cluster_intersecting_on_polygon_field_produces_aggregate_vec_multipolygon() {
        use crate::expr::AggregateExpr;
        use crate::expr::node::AggOp;
        use crate::geo::{MultiPolygon, Polygon as PolygonTy};

        let field: FieldRef<Fake, PolygonTy> = FieldRef::new("territory");
        // Compile-time return-type check — Vec<MultiPolygon>.
        let agg: AggregateExpr<Vec<MultiPolygon>> = field.cluster_intersecting();
        if let ExprNode::Aggregate { op, arg, .. } = agg.node {
            assert!(matches!(op, AggOp::SpatialClusterIntersecting));
            if let ExprNode::Field { column } = *arg {
                assert_eq!(column, "territory");
                return;
            }
            panic!("expected Aggregate.arg to wrap the column");
        }
        panic!("expected ExprNode::Aggregate(SpatialClusterIntersecting)");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn cluster_intersecting_emits_st_clusterintersecting_with_geography_array_cast() {
        use crate::geo::Polygon as PolygonTy;
        use crate::pg::accumulator::SqlAccumulator;
        let field: FieldRef<Fake, PolygonTy> = FieldRef::new("territory");
        let agg = field.cluster_intersecting();
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        assert_eq!(
            acc.sql(),
            "ST_ClusterIntersecting(territory::geometry)::geography[]"
        );
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn cluster_within_carries_distance_inline_on_aggop() {
        use crate::expr::AggregateExpr;
        use crate::expr::node::AggOp;
        use crate::geo::{MultiPolygon, Polygon as PolygonTy};

        let field: FieldRef<Fake, PolygonTy> = FieldRef::new("territory");
        let agg: AggregateExpr<Vec<MultiPolygon>> = field.cluster_within(1_000.0);
        if let ExprNode::Aggregate { op, .. } = agg.node {
            // Distance is carried inline on the variant — assert the
            // variant matches and pin the value.
            if let AggOp::SpatialClusterWithin(distance) = op {
                assert!(
                    (distance - 1_000.0).abs() < f64::EPSILON,
                    "distance must round-trip; got {distance}"
                );
                return;
            }
            panic!("expected AggOp::SpatialClusterWithin variant");
        }
        panic!("expected ExprNode::Aggregate");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn cluster_within_emits_st_clusterwithin_with_distance_bound() {
        // Distance binds as a parameter, not inlined into SQL text —
        // verifies no string interpolation of user-supplied data.
        use crate::geo::Polygon as PolygonTy;
        use crate::pg::accumulator::SqlAccumulator;
        let field: FieldRef<Fake, PolygonTy> = FieldRef::new("territory");
        let agg = field.cluster_within(500.0);
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        assert_eq!(
            acc.sql(),
            "ST_ClusterWithin(territory::geometry, $1)::geography[]"
        );
        assert_eq!(acc.bind_count(), 1, "expected 1 bind for the distance");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn cluster_intersecting_on_multipolygon_field_also_dispatches() {
        use crate::expr::AggregateExpr;
        use crate::geo::MultiPolygon;
        let field: FieldRef<Fake, MultiPolygon> = FieldRef::new("region");
        let _agg: AggregateExpr<Vec<MultiPolygon>> = field.cluster_intersecting();
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn cluster_within_on_multipolygon_field_also_dispatches() {
        use crate::expr::AggregateExpr;
        use crate::geo::MultiPolygon;
        let field: FieldRef<Fake, MultiPolygon> = FieldRef::new("region");
        let _agg: AggregateExpr<Vec<MultiPolygon>> = field.cluster_within(2_000.0);
    }

    // ── T16 — mem_union / polygonize ─────────────────────────────────────────

    #[cfg(feature = "spatial")]
    #[test]
    fn mem_union_on_polygon_field_produces_aggregate_multipolygon() {
        use crate::expr::AggregateExpr;
        use crate::expr::node::AggOp;
        use crate::geo::{MultiPolygon, Polygon as PolygonTy};

        let field: FieldRef<Fake, PolygonTy> = FieldRef::new("territory");
        let agg: AggregateExpr<MultiPolygon> = field.mem_union();
        if let ExprNode::Aggregate { op, arg, .. } = agg.node {
            assert!(matches!(op, AggOp::SpatialMemUnion));
            if let ExprNode::Field { column } = *arg {
                assert_eq!(column, "territory");
                return;
            }
            panic!("expected Aggregate.arg to wrap the column");
        }
        panic!("expected ExprNode::Aggregate(SpatialMemUnion)");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn mem_union_emits_st_memunion_with_geography_cast() {
        use crate::geo::Polygon as PolygonTy;
        use crate::pg::accumulator::SqlAccumulator;
        let field: FieldRef<Fake, PolygonTy> = FieldRef::new("territory");
        let agg = field.mem_union();
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "ST_MemUnion(territory::geometry)::geography");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn mem_union_on_multipolygon_field_also_dispatches() {
        use crate::expr::AggregateExpr;
        use crate::geo::MultiPolygon;
        let field: FieldRef<Fake, MultiPolygon> = FieldRef::new("region");
        let _agg: AggregateExpr<MultiPolygon> = field.mem_union();
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn polygonize_on_linestring_field_produces_aggregate_multipolygon() {
        use crate::expr::AggregateExpr;
        use crate::expr::node::AggOp;
        use crate::geo::{LineString, MultiPolygon};

        let field: FieldRef<Fake, LineString> = FieldRef::new("edge");
        let agg: AggregateExpr<MultiPolygon> = field.polygonize();
        if let ExprNode::Aggregate { op, arg, .. } = agg.node {
            assert!(matches!(op, AggOp::SpatialPolygonize));
            if let ExprNode::Field { column } = *arg {
                assert_eq!(column, "edge");
                return;
            }
            panic!("expected Aggregate.arg to wrap the column");
        }
        panic!("expected ExprNode::Aggregate(SpatialPolygonize)");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn polygonize_emits_st_polygonize_with_geography_cast() {
        use crate::geo::LineString;
        use crate::pg::accumulator::SqlAccumulator;
        let field: FieldRef<Fake, LineString> = FieldRef::new("edge");
        let agg = field.polygonize();
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "ST_Polygonize(edge::geometry)::geography");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn polygonize_with_distinct_emits_distinct_inside_st_polygonize() {
        use crate::geo::LineString;
        use crate::pg::accumulator::SqlAccumulator;
        let field: FieldRef<Fake, LineString> = FieldRef::new("edge");
        let agg = field.polygonize().distinct();
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        assert_eq!(
            acc.sql(),
            "ST_Polygonize(DISTINCT edge::geometry)::geography"
        );
    }

    // ── line_agg — T14b retroactive completion ───────────────────────────────

    #[cfg(feature = "spatial")]
    #[test]
    fn line_agg_on_linestring_field_produces_aggregate_multilinestring() {
        use crate::expr::AggregateExpr;
        use crate::expr::node::AggOp;
        use crate::geo::{LineString, MultiLineString};

        let field: FieldRef<Fake, LineString> = FieldRef::new("path");
        let agg: AggregateExpr<MultiLineString> = field.line_agg();
        if let ExprNode::Aggregate { op, arg, .. } = agg.node {
            assert!(matches!(op, AggOp::SpatialLineAgg));
            if let ExprNode::Field { column } = *arg {
                assert_eq!(column, "path");
                return;
            }
            panic!("expected Aggregate.arg to wrap the column");
        }
        panic!("expected ExprNode::Aggregate(SpatialLineAgg)");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn line_agg_emits_st_lineagg_with_geography_cast() {
        use crate::geo::LineString;
        use crate::pg::accumulator::SqlAccumulator;
        let field: FieldRef<Fake, LineString> = FieldRef::new("path");
        let agg = field.line_agg();
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "ST_LineAgg(path::geometry)::geography");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn line_agg_with_distinct_emits_distinct_inside_st_lineagg() {
        use crate::geo::LineString;
        use crate::pg::accumulator::SqlAccumulator;
        let field: FieldRef<Fake, LineString> = FieldRef::new("path");
        let agg = field.line_agg().distinct();
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "ST_LineAgg(DISTINCT path::geometry)::geography");
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn line_agg_with_filter_attaches_before_cast() {
        // Codex T22 BLOCK-1: FILTER must precede the ::geography cast.
        // Correct shape:
        //   (ST_LineAgg(<col>::geometry) FILTER (WHERE <cond>))::geography
        use crate::expr::Expr;
        use crate::geo::LineString;
        use crate::pg::accumulator::SqlAccumulator;
        let path: FieldRef<Fake, LineString> = FieldRef::new("path");
        let len_m: FieldRef<Fake, f64> = FieldRef::new("length_m");
        let agg = path
            .line_agg()
            .filter(len_m.as_expr().gt(Expr::literal(100.0_f64)));
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        let sql = acc.sql().to_string();
        assert!(
            sql.starts_with("(ST_LineAgg(path::geometry)"),
            "must start with (ST_LineAgg for FILTER attachment, got: {sql}"
        );
        assert!(
            sql.contains(" FILTER (WHERE length_m > "),
            "FILTER clause must be present, got: {sql}"
        );
        assert!(
            sql.ends_with(")::geography"),
            "::geography cast must close after FILTER, got: {sql}"
        );
        let filter_idx = sql.find(" FILTER (WHERE").unwrap();
        let cast_idx = sql.rfind("::geography").unwrap();
        assert!(
            filter_idx < cast_idx,
            "FILTER must precede ::geography cast for valid Postgres syntax; got: {sql}"
        );
    }

    // area_of / area_of_intersection typed surface tests

    /// `Expr::area_of(&geom)` must produce an `Expr<f64>` wrapping the
    /// `SpatialExpr::Area` IR variant with the EWKB bytes captured.
    #[cfg(feature = "spatial")]
    #[test]
    fn area_of_produces_f64_expr_with_area_node() {
        let poly = make_polygon();
        // Compile-time return-type check.
        let expr: crate::expr::Expr<f64> = crate::expr::Expr::area_of(&poly);
        if let ExprNode::Spatial(SpatialExpr::Area { geom_ewkb }) = expr.node {
            // EWKB encoding must be non-empty — captured from the GeographyValue.
            assert!(!geom_ewkb.is_empty(), "Area must capture EWKB bytes");
            return;
        }
        panic!("expected ExprNode::Spatial(SpatialExpr::Area {{..}})");
    }

    /// `Expr::area_of_intersection(&a, &b)` must produce the fused
    /// `SpatialExpr::AreaOfIntersection` IR variant. Composes with arithmetic
    /// for the territory-overlap-percentage demo (`area(intersection(a, b)) /
    /// area(a)` → ratio in `[0.0, 1.0]`).
    #[cfg(feature = "spatial")]
    #[test]
    fn area_of_intersection_produces_f64_expr_with_fused_node() {
        let a = make_polygon();
        let b = make_polygon();
        let expr: crate::expr::Expr<f64> = crate::expr::Expr::area_of_intersection(&a, &b);
        if let ExprNode::Spatial(SpatialExpr::AreaOfIntersection { a_ewkb, b_ewkb }) = expr.node {
            assert!(!a_ewkb.is_empty(), "a EWKB captured");
            assert!(!b_ewkb.is_empty(), "b EWKB captured");
            return;
        }
        panic!("expected ExprNode::Spatial(SpatialExpr::AreaOfIntersection {{..}})");
    }

    /// The composed ratio `area_of_intersection(a, b) / area_of(a)` lowers to
    /// an `Expr::Div(Spatial(AreaOfIntersection), Spatial(Area))` shape via the
    /// existing `Numeric` arithmetic IR — verifies the demo's
    /// territory-overlap-percentage call site type-checks end-to-end.
    #[cfg(feature = "spatial")]
    #[test]
    fn area_of_intersection_divides_by_area_of_for_overlap_pct() {
        use crate::expr::Expr;
        let a = make_polygon();
        let b = make_polygon();
        let ratio: Expr<f64> = Expr::area_of_intersection(&a, &b) / Expr::area_of(&a);
        // The outer node must be Div; both operands must wrap Spatial nodes.
        if let ExprNode::Div(lhs, rhs) = ratio.node
            && matches!(
                *lhs,
                ExprNode::Spatial(SpatialExpr::AreaOfIntersection { .. })
            )
            && matches!(*rhs, ExprNode::Spatial(SpatialExpr::Area { .. }))
        {
            return;
        }
        panic!("expected Div(Spatial(AreaOfIntersection), Spatial(Area))");
    }

    // Narrow-integer IntoFilterValue widening (Phase 7-Zero-2 polish,
    // GH issue #29). Each narrow type widens to the smallest signed
    // FilterValue variant that fits its full range. Mirrors the
    // sql_cast_for_type table in `jsonb::path`.
    #[test]
    fn into_filter_value_i8_widens_to_i16() {
        match (-1i8).into_filter_value() {
            FilterValue::I16(v) => assert_eq!(v, -1),
            other => panic!("expected I16, got {other:?}"),
        }
        match i8::MAX.into_filter_value() {
            FilterValue::I16(v) => assert_eq!(v, 127),
            other => panic!("expected I16, got {other:?}"),
        }
    }

    #[test]
    fn into_filter_value_u8_widens_to_i16() {
        match 0u8.into_filter_value() {
            FilterValue::I16(v) => assert_eq!(v, 0),
            other => panic!("expected I16, got {other:?}"),
        }
        match u8::MAX.into_filter_value() {
            // u8 max 255 fits in i16 without overflow.
            FilterValue::I16(v) => assert_eq!(v, 255),
            other => panic!("expected I16, got {other:?}"),
        }
    }

    #[test]
    fn into_filter_value_u16_widens_to_i32() {
        match 0u16.into_filter_value() {
            FilterValue::I32(v) => assert_eq!(v, 0),
            other => panic!("expected I32, got {other:?}"),
        }
        match u16::MAX.into_filter_value() {
            // u16 max 65535 exceeds i16 max 32767, so widen to i32.
            FilterValue::I32(v) => assert_eq!(v, 65_535),
            other => panic!("expected I32, got {other:?}"),
        }
    }

    #[test]
    fn into_filter_value_u32_widens_to_i64() {
        match 0u32.into_filter_value() {
            FilterValue::I64(v) => assert_eq!(v, 0),
            other => panic!("expected I64, got {other:?}"),
        }
        match u32::MAX.into_filter_value() {
            // u32 max ~4.3B exceeds i32 max ~2.1B, so widen to i64.
            FilterValue::I64(v) => assert_eq!(v, 4_294_967_295),
            other => panic!("expected I64, got {other:?}"),
        }
    }
}
