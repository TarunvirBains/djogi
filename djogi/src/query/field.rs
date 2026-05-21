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
//!   Rust type, so `view_count.eq("hello")` fails at the field-argument
//!   conversion bound — again at compile time.
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
//!     f.published().eq(true) & f.view_count().gte(100)
//! });
//! ```
//!
//! Use `&` / `|` / `!` for composition — both portable
//! [`PortablePredicate`](crate::query::PortablePredicate) and SQL-only
//! [`Condition`](crate::query::internal::Condition) honor the operator
//! traits, so a single closure can mix the two without naming either type.
//! For SQL-only predicates, the [`ConditionExt`](crate::query::ConditionExt)
//! trait (in scope through the prelude) additionally exposes `.and(...)` /
//! `.or(...)` method-chain forms; negation stays on the unary `!` operator
//! and the associated function
//! [`Condition::not`](crate::query::internal::Condition::not).
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
use crate::query::predicate::PortablePredicate;
use crate::tracked::Tracked;
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

// ── Phase 8eta PR2a — DjogiField root field wrapper ─────────────────────────
//
// `DjogiField<M, V>` is the root field wrapper that PR3 will flip the
// `{Model}Fields` macro accessors over to. It carries both halves Djogi
// needs:
//
// - `portable: sassi::Field<M, V>` — the in-memory predicate accessor
//   `PunnuScope::filter_basic` evaluates against `&M`.
// - `sql: FieldRef<M, V>` — the path-aware SQL handle the existing emitter
//   already understands.
//
// PR2a is deliberately **additive**: the type and its methods exist, but
// nothing in the framework constructs `DjogiField` values yet. PR3 flips
// the generated root accessors; PR2b/PR2d wire SQL emission; PR4 hooks the
// cache boundary. Splitting this way keeps every PR independently
// compilable.
//
// # Why not just expose raw `sassi::Field<M, V>`?
//
// Two reasons. First, `sassi::Field::new("any_string", arbitrary_extractor)`
// is `pub` — downstream code can construct a `Field` whose name doesn't
// match any real column on `M`. Routing every Djogi root predicate through
// `DjogiField` lets PR2a's [`PortablePredicate::from_djogi_field`] enforce
// the trusted-provenance invariant at the wrapper boundary. Second, raw
// Sassi string predicates have **case-sensitive** `contains` semantics
// while existing Djogi `FieldRef::contains` is **case-insensitive**.
// Exposing Sassi fields directly would silently flip those semantics on
// adopters porting between in-memory and SQL filters. `DjogiField` keeps
// the existing Djogi spelling and routes case-sensitive matching through
// explicit `contains_case_sensitive` / `explicit_pg_predicate()` opt-ins.

/// Trusted-construction marker for portable predicates.
///
/// Constructed only by `DjogiField` / `DjogiPresentField` predicate methods
/// inside this module. Carrying the marker as an argument to
/// [`PortablePredicate::from_djogi_field`] makes the trusted-provenance
/// invariant visible to the type checker — crate-internal code that
/// accidentally imports `sassi::BasicPredicate` and tries to wrap it would
/// have to construct a `DjogiFieldProvenance` first, which is unreachable
/// outside this module.
///
/// The struct deliberately has a private field so even crate-internal code
/// outside `crate::query::field` cannot fabricate an instance.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct DjogiFieldProvenance {
    // Private field — only `mint_provenance()` can populate it.
    _seal: (),
}

#[doc(hidden)]
impl DjogiFieldProvenance {
    /// Mint a fresh provenance marker. Module-private — only the
    /// `DjogiField` / `DjogiPresentField` predicate-builder methods below
    /// reach this constructor. Crate-internal code outside `query::field`
    /// is blocked because the function is `fn(...)` rather than `pub(...) fn`,
    /// and the struct's only field is private.
    fn mint_provenance() -> Self {
        Self { _seal: () }
    }

    /// Mint a provenance marker for the MirJzSON JSON predicate builder
    /// (`query::mirjzson::DjogiField<M, MirJzSON>::jsahibon()`).
    ///
    /// `pub(crate)` so the sibling `query::mirjzson` module can reach
    /// it; crate-internal code outside `query::*` cannot reach the
    /// constructor because `DjogiFieldProvenance`'s only field is
    /// private. The MirJzSON-specific name documents the single
    /// legitimate caller and keeps the trust surface auditable in
    /// `grep`.
    pub(crate) fn __mirjzson_mint() -> Self {
        Self::mint_provenance()
    }
}

/// Marker trait for value types whose direct portable ordering methods
/// (`gt`/`gte`/`lt`/`lte`/`between`) are exposed on
/// [`DjogiField<M, V>`](DjogiField).
///
/// # What and why
///
/// Djogi exposes ordering only on types whose Rust ordering matches the
/// SQL ordering Djogi emits. Implementing the trait is the explicit opt-in
/// — PR2a populates it for the safe scalar types Djogi already binds
/// through `IntoFilterValue` (signed integers + Decimal + HeerId/RanjId
/// families), and adopter newtypes that satisfy the bind/clone bounds can
/// add an impl per type.
///
/// # Deliberate exclusions
///
/// - **`String`**: Postgres text ordering depends on the database's
///   collation, which doesn't match Rust's byte-lexicographic `Ord`.
///   Adopters who want database-locale text ordering reach for
///   `explicit_pg_predicate().gt(...)` until a future phase pins
///   collation-aware portable ordering.
/// - **`f32` / `f64`**: SQL `NULLS FIRST/LAST` and IEEE-754 NaN ordering
///   diverge from Rust's `PartialOrd` semantics. A future phase may add a
///   collation-pinned float ordering with explicit NaN handling.
/// - **`Option<U>`**: Rust's `Option` ordering (`None < Some(_)`) doesn't
///   match SQL three-valued NULL semantics. Callers use `.some().gt(v)`
///   instead.
/// - **No blanket impl**: a `impl<T: PartialOrd + ToSql + …>` would
///   silently include `Option<U>` and any future foreign type.
///
/// # Adopter extension
///
/// Custom scalar types that bind through `postgres_types::ToSql` and whose
/// Rust `Ord` matches the SQL ordering Djogi emits can opt in:
///
/// ```ignore
/// impl djogi::query::DjogiPortableOrd for MyType {}
/// ```
///
/// `MyType` must already satisfy `PartialOrd + postgres_types::ToSql + Clone +
/// Send + Sync + 'static`. If it does not satisfy the bind/clone surface,
/// PR2d's generated SQL lowering returns `UnsupportedFieldType`.
pub trait DjogiPortableOrd:
    PartialOrd + postgres_types::ToSql + Clone + Send + Sync + 'static
{
}

/// Marker trait for value types whose direct portable equality methods
/// (`eq`/`neq`/`in_`/`not_in`) are exposed on
/// [`DjogiField<M, V>`](DjogiField).
///
/// Equality is only portable when Rust `PartialEq` agrees with the SQL
/// equality operator Djogi emits. `Interval` is deliberately excluded:
/// PostgreSQL `INTERVAL =` linearizes months/days/microseconds before
/// comparing, while [`crate::Interval`] equality is structural. `f32`/`f64`
/// are also excluded because PostgreSQL treats `NaN` equality differently
/// from Rust/Punnu.
pub trait DjogiPortableEq:
    PartialEq + postgres_types::ToSql + Clone + Send + Sync + 'static
{
}

// Explicit impls for built-in scalar types whose Rust ordering matches the
// SQL ordering Djogi emits. PR2a opts these in; PR3 populates the
// generated `DjogiField` ordering callsites once macros flip.
//
// Coverage rationale per type:
// - Signed integers `i8`/`i16`/`i32`/`i64`: Postgres int2/int4/int8 ordering
//   is numeric and matches Rust `Ord`. `i8` directly satisfies
//   `postgres_types::ToSql` (binds as int2 with range-checked widening).
// - `u32`: directly satisfies `postgres_types::ToSql` (binds as `oid`).
//   `u8` / `u16` do not have shipped `ToSql` impls, so they cannot enter
//   `DjogiPortableOrd`'s `ToSql` supertrait; adopters who model fields
//   as `u8` / `u16` widen at the column type or reach the legacy
//   `IntoFilterValue` widening through `FieldRef` / `explicit_pg_predicate`.
// - `time::OffsetDateTime` / `time::Date`: monotone numeric encoding under
//   Postgres' `timestamptz` / `date`.
// - `uuid::Uuid`: byte-lexicographic and matches Rust `Ord`.
// - Primary-key types: covered by the sealed `PrimaryKey` blanket below.
//   Built-in HeerId / RanjId families remain portable because their Rust
//   equality matches their Postgres bigint / uuid equality; custom
//   `primary_key!` newtypes inherit their inner scalar equality semantics.
// - `rust_decimal::Decimal`: numeric ordering under Postgres `numeric`.
//
// Deliberately omitted: `bool` (no ordering callers), `String`, `f32`,
// `f64`, `Option<U>`, `u8`, `u16`, `u64`. See trait docs. `u64` is not
// `postgres_types::ToSql` and so cannot satisfy the `DjogiPortableOrd`
// supertrait bounds today — the `IntoFilterValue for u64` impl above
// routes through `rust_decimal::Decimal` for the SQL bind side, but
// the portable-ord trait requires the Rust type bind directly. When
// djogi#190 wires per-field bind shims, a `u64` portable-ord becomes
// reachable via the same chained-widening route Decimal already has.

impl DjogiPortableOrd for i8 {}
impl DjogiPortableOrd for i16 {}
impl DjogiPortableOrd for i32 {}
impl DjogiPortableOrd for i64 {}
impl DjogiPortableOrd for u32 {}
impl DjogiPortableOrd for time::OffsetDateTime {}
impl DjogiPortableOrd for time::Date {}
impl DjogiPortableOrd for uuid::Uuid {}
impl DjogiPortableOrd for crate::HeerId {}
impl DjogiPortableOrd for crate::RanjId {}
impl DjogiPortableOrd for crate::HeerIdDesc {}
impl DjogiPortableOrd for crate::RanjIdDesc {}
impl DjogiPortableOrd for rust_decimal::Decimal {}
impl<V> DjogiPortableOrd for Tracked<V> where V: DjogiPortableOrd {}

impl DjogiPortableEq for String {}
impl DjogiPortableEq for i8 {}
impl DjogiPortableEq for i16 {}
impl DjogiPortableEq for i64 {}
impl DjogiPortableEq for u32 {}
impl DjogiPortableEq for bool {}
impl DjogiPortableEq for time::OffsetDateTime {}
impl DjogiPortableEq for time::Date {}
impl DjogiPortableEq for uuid::Uuid {}
impl DjogiPortableEq for rust_decimal::Decimal {}
// djogi#213 — network family. Rust structural `PartialEq` on these
// types DOES agree byte-for-byte with Postgres `=` (unlike `Interval`,
// whose Postgres `=` linearizes months/days). However, the network
// types are deliberately NOT routed through `DjogiPortableEq` /
// the generic portable predicate path. Reason: enabling the portable
// path requires the macro classifier in `portable_field_emit.rs` to
// recognise the type names as `Scalar`, which would cascade into
// macro-emitted `where V: DjogiPortableEq` bounds for every model
// field of those types. The macro crate cannot conditionally classify
// based on the runtime crate's feature flags, so unconditionally
// classifying network types as Scalar would fail to compile with
// `network` off (the trait bound would fail at model expansion).
// Routing through SQL-only DjogiField impls (below) instead keeps
// the model expansion working regardless of feature state — the
// network typed surface is reachable only when the feature is on
// because that's when the field types themselves exist; the macro
// classifier sees them as Unsupported and emits the catch-all
// portable arm, while the explicit SQL-only impls below provide
// `.eq` / `.neq` / `.in_` / `.not_in` directly. This is the same
// pattern `Interval` uses.
impl<V> DjogiPortableEq for V where
    V: crate::primary_key::PrimaryKey
        + PartialEq
        + postgres_types::ToSql
        + Clone
        + Send
        + Sync
        + 'static
{
}
impl<V> DjogiPortableEq for Option<V>
where
    V: DjogiPortableEq,
    Option<V>: postgres_types::ToSql,
{
}
impl<V> DjogiPortableEq for Tracked<V> where V: DjogiPortableEq {}

/// Marker for array element types whose `Vec<T>` equality has been
/// parity-checked between Rust/Punnu and PostgreSQL.
///
/// `IntoArrayFilterValue` is intentionally wider: it also contains
/// SQL-bindable array element types whose scalar equality is not portable
/// enough for Punnu-backed predicates, notably floats. Keep this marker
/// curated so SQL-only array operators can remain broad while direct
/// portable equality/membership on `Vec<T>` stays parity-safe.
#[doc(hidden)]
pub trait DjogiPortableArrayEqElement: IntoArrayFilterValue + DjogiPortableEq {}

impl DjogiPortableArrayEqElement for String {}
impl DjogiPortableArrayEqElement for i16 {}
impl DjogiPortableArrayEqElement for i32 {}
impl DjogiPortableArrayEqElement for i64 {}
impl DjogiPortableArrayEqElement for bool {}
impl DjogiPortableArrayEqElement for time::OffsetDateTime {}
impl DjogiPortableArrayEqElement for time::Date {}
impl DjogiPortableArrayEqElement for uuid::Uuid {}
impl DjogiPortableArrayEqElement for rust_decimal::Decimal {}
impl DjogiPortableArrayEqElement for crate::types::HeerId {}
impl DjogiPortableArrayEqElement for crate::types::RanjId {}
impl DjogiPortableArrayEqElement for crate::types::HeerIdDesc {}
impl DjogiPortableArrayEqElement for crate::types::RanjIdDesc {}

impl<V> DjogiPortableEq for Vec<V>
where
    V: DjogiPortableArrayEqElement,
    Vec<V>: postgres_types::ToSql + Clone + Send + Sync + 'static,
{
}

/// Djogi root field wrapper.
///
/// Carries the Sassi `Field<M, V>` (used by Punnu in-memory evaluation) and
/// the Djogi `FieldRef<M, V>` (used by SQL emission) so one root accessor
/// can compose portable predicates and database queries from the same
/// closure shape.
///
/// PR2a defines the type and its method surface. PR3 will flip macro-
/// generated `{Model}Fields` accessors from `FieldRef` over to `DjogiField`.
/// Until that flip, `DjogiField` is reachable through the public re-export
/// in [`crate::query`](crate::query) but is not the return type of any
/// generated accessor.
///
/// # Method semantics
///
/// | Method family            | Receiver                       | Returns                      |
/// |--------------------------|--------------------------------|------------------------------|
/// | `eq`, `neq`              | `DjogiField<M, V>` where `V: DjogiPortableEq` | `PortablePredicate<M>` |
/// | `gt`/`gte`/`lt`/`lte`    | `DjogiField<M, V>` where `V: DjogiPortableOrd` | `PortablePredicate<M>` |
/// | `between`                | `DjogiField<M, V>` where `V: DjogiPortableOrd` | `PortablePredicate<M>` |
/// | `in_`/`not_in`           | `DjogiField<M, V>` where `V: DjogiPortableEq` | `PortablePredicate<M>` |
/// | `is_null`/`is_not_null`  | `DjogiField<M, Option<U>>`     | `PortablePredicate<M>`       |
/// | `some()`                 | `DjogiField<M, Option<U>>`     | `DjogiPresentField<M, U>`    |
/// | `contains`/`icontains`   | `DjogiField<M, String>`        | `PortablePredicate<M>` (ASCII-stable case-insensitive) |
/// | `starts_with`/`ends_with`| `DjogiField<M, String>`        | `PortablePredicate<M>` (ASCII-stable case-insensitive) |
/// | `*_case_sensitive` family| `DjogiField<M, String>`        | `PortablePredicate<M>`       |
/// | `iexact`                 | `DjogiField<M, String>`        | `PortablePredicate<M>`       |
/// | `explicit_pg_predicate()`| `DjogiField<M, V>`             | `ExplicitPgPredicateField<M, V>` |
/// | non-predicate SQL helpers| `DjogiField<M, V>`             | forwarded to `FieldRef`      |
///
/// PostgreSQL-specific predicates (regex, JSONB path, FTS, spatial, array
/// operators, expression-producing predicates) are reached through
/// [`DjogiField::explicit_pg_predicate`] and return ordinary
/// [`Condition`] / [`crate::expr::Expr<bool>`] values. They are valid
/// database queries but rejected by cache/refresh boundaries — see the
/// `ExplicitPgPredicateField` docs for the rationale and routing.
pub struct DjogiField<M: Model, V> {
    portable: sassi::Field<M, V>,
    sql: FieldRef<M, V>,
    /// Memory-side extractor — `fn(&M) -> &V` function pointer captured by
    /// the model macro at `#[model]` expansion time. Duplicates the same
    /// pointer that lives inside `portable` (Sassi keeps that field
    /// `pub(crate)` in its own crate, so we cannot read it from here).
    ///
    /// Needed by the MirJzSON `.jsahibon()` lift path
    /// (`crate::query::mirjzson::DjogiField<M, MirJzSON>::jsahibon`): the
    /// lift transmutes this pointer to `fn(&M) -> &sassi::JSahibON` under
    /// `MirJzSON`'s `#[repr(transparent)]` invariant, then constructs a
    /// fresh `sassi::Field<M, JSahibON>` so Sassi's typed predicate
    /// builder chain can take over.
    ///
    /// `pub(crate)` so the broader query module can read it without
    /// exposing the raw pointer in the public API.
    pub(crate) extractor: fn(&M) -> &V,
}

/// Optional-value present-only predicate view.
///
/// Returned by [`DjogiField::some`] on `DjogiField<M, Option<U>>`. Exposes
/// the same `eq`/`neq`/`in_`/`not_in`/`gt`/`gte`/`lt`/`lte`/`between`
/// surface as `DjogiField<M, U>`, but every predicate evaluates `None` as
/// `false` and emits SQL that excludes NULL rows.
///
/// PR2a defines the type so portable optional comparisons compose through
/// the Djogi `&`/`|` operators rather than dropping back to raw Sassi
/// predicates. PR2b/PR2d add the matching SQL emission.
pub struct DjogiPresentField<M: Model, V> {
    portable: sassi::PresentField<M, V>,
    sql: FieldRef<M, V>,
}

/// PostgreSQL-specific predicate view of a root field.
///
/// Returned by [`DjogiField::explicit_pg_predicate`]. Exposes the existing
/// `FieldRef` predicate surface that is **not** portable to Punnu —
/// regex/iregex, database-locale string pattern predicates, JSONB path /
/// typed predicates, array operators, all current spatial/PostGIS
/// predicates, and expression-only predicate chains. The wrapper deliberately
/// returns ordinary `Condition` / `Expr<bool>` values (not
/// `PortablePredicate<M>`) so cache and refresh boundaries reject them
/// through PR4's portability gate.
///
/// The `explicit_pg_predicate()` name was chosen over `.sql()` / `.db()` /
/// `.pg()` because adopters reading `f.title().contains("rust")` should not
/// infer that ordinary database queries require a route — only PostgreSQL-
/// specific semantics do.
///
/// PR2a forwards the existing `FieldRef` predicate surface. PR3 widens the
/// set as the macro flip reveals additional methods.
pub struct ExplicitPgPredicateField<M: Model, V> {
    sql: FieldRef<M, V>,
}

// `Copy` / `Clone` impls are manual rather than derive: derive would impose
// `M: Copy` / `V: Copy` (because `DjogiField<M, V>` carries
// `sassi::Field<M, V>` whose `PhantomData<(M, V)>` field "owns" the type
// parameters in the derive's view). Manual impls match the existing
// `FieldRef<M, V>` pattern: every real-data field is `Copy`, so the wrapper
// is `Copy` regardless of `M` / `V`.

impl<M: Model, V> Copy for DjogiField<M, V> {}
impl<M: Model, V> Clone for DjogiField<M, V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M: Model, V> std::fmt::Debug for DjogiField<M, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DjogiField")
            .field("column", &self.sql.column())
            .finish_non_exhaustive()
    }
}

impl<M: Model, V> Copy for DjogiPresentField<M, V> {}
impl<M: Model, V> Clone for DjogiPresentField<M, V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M: Model, V> std::fmt::Debug for DjogiPresentField<M, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DjogiPresentField")
            .field("column", &self.sql.column())
            .finish_non_exhaustive()
    }
}

impl<M: Model, V> Copy for ExplicitPgPredicateField<M, V> {}
impl<M: Model, V> Clone for ExplicitPgPredicateField<M, V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M: Model, V> std::fmt::Debug for ExplicitPgPredicateField<M, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExplicitPgPredicateField")
            .field("column", &self.sql.column())
            .finish_non_exhaustive()
    }
}

// ── DjogiField — common accessors ─────────────────────────────────────────

impl<M: Model, V> DjogiField<M, V> {
    /// Return the underlying portable `sassi::Field<M, V>`.
    ///
    /// **Crate-private internal accessor.** Direct Sassi `Field` string
    /// methods have case-sensitive semantics that don't match Djogi's
    /// portable ASCII-stable case-insensitive contract. The framework's
    /// SQL emitter and in-crate test helpers use this accessor; adopter
    /// code should compose through `DjogiField` methods instead.
    ///
    /// PR2d's macro-emitted `Model::__djogi_emit_field_predicate` overrides
    /// receive a `&FieldPredicate<Self>` directly and never unpack a
    /// `DjogiField`, so this accessor stays `pub(crate)` rather than `pub`
    /// to keep the trusted-construction surface tight.
    #[doc(hidden)]
    pub(crate) fn __portable_field(self) -> sassi::Field<M, V> {
        self.portable
    }

    /// Return the underlying SQL `FieldRef<M, V>`.
    ///
    /// **Crate-private internal accessor.** The wrapper exposes
    /// non-predicate SQL helpers (`as_expr`, ordering, aggregates, etc.)
    /// directly through forwarded methods on `DjogiField`; adopters do
    /// not need to reach for the inner ref. Only PR2b's in-crate SQL
    /// walker and in-crate test helpers consume this accessor.
    ///
    /// PR2d's macro-emitted overrides spell columns through bare `&'static`
    /// names threaded into the helper signatures (`emit_value::<M, V>(acc,
    /// ctx, "column", "=", field)`); they never reach for `__sql_field` to
    /// extract a `FieldRef`, so `pub(crate)` is sufficient.
    #[doc(hidden)]
    pub(crate) fn __sql_field(self) -> FieldRef<M, V> {
        self.sql
    }

    /// Enter the PostgreSQL-specific predicate surface for this root field.
    ///
    /// Predicates produced through this view (regex, ILIKE database-locale
    /// patterns, JSONB path, FTS, spatial, array operators, expression
    /// predicates) emit valid SQL but are rejected by Djogi cache and
    /// refresh boundaries because they cannot be evaluated in Punnu.
    ///
    /// This is the **only** root-field route to PostgreSQL-specific
    /// predicate methods — direct names like `regex` / JSONB path / etc.
    /// are not exposed on `DjogiField` itself. Adopters reading
    /// `f.title().contains("rust")` get the portable ASCII-stable case-
    /// insensitive contains; database-locale `ILIKE` lives on
    /// `f.title().explicit_pg_predicate().contains("é")`.
    #[must_use = "ExplicitPgPredicateField is lazy — drop it and the predicate is omitted"]
    pub fn explicit_pg_predicate(self) -> ExplicitPgPredicateField<M, V> {
        ExplicitPgPredicateField { sql: self.sql }
    }

    // ── Non-predicate SQL helpers — forward directly to FieldRef ────────────
    //
    // These helpers are SQL-only by nature and don't enter the portable
    // predicate boundary. Forwarding lets adopters stay on `DjogiField`
    // for the entire chain rather than threading `FieldRef` back out.

    /// Promote into the expression IR — see [`FieldRef::as_expr`].
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn as_expr(self) -> crate::expr::Expr<V> {
        self.sql.as_expr()
    }

    /// Ascending ordering for this column. Forwarded from [`FieldRef::asc`].
    #[must_use = "order expressions are inert until passed to `order_by`"]
    pub fn asc(self) -> crate::query::order::OrderExpr {
        self.sql.asc()
    }

    /// Descending ordering for this column. Forwarded from [`FieldRef::desc`].
    #[must_use = "order expressions are inert until passed to `order_by`"]
    pub fn desc(self) -> crate::query::order::OrderExpr {
        self.sql.desc()
    }

    /// Internal accessor — column name, mirrors [`FieldRef::column`].
    #[doc(hidden)]
    pub fn column(self) -> &'static str {
        self.sql.column()
    }
}

impl<M: Model, V: IntoFilterValue> DjogiField<M, V> {
    /// Build a typed `SET column = value` assignment.
    #[must_use = "assignments are lazy — drop one and the SET clause is silently omitted"]
    pub fn set(self, value: V) -> crate::query::update::UpdateAssignment {
        self.sql.set(value)
    }

    /// Build an expression-backed `SET column = <expr>` assignment.
    #[must_use = "assignments are lazy — drop one and the SET clause is silently omitted"]
    pub fn set_expr(self, expr: crate::expr::Expr<V>) -> crate::query::update::UpdateAssignment {
        self.sql.set_expr(expr)
    }
}

// Phase 8.5 Cluster 4B (djogi#106) — INSERT...SELECT column mapping.
//
// The macro-emitted `{Model}Fields` accessors return `DjogiField<M, V>`
// after Phase 8eta PR3, so the typed INSERT...SELECT column-mapping
// builder must surface here in addition to the underlying `FieldRef`
// impl (`FieldRef::copy_from` / `FieldRef::as_insert_source` in
// `query::insert_select`). The forwarding pattern mirrors
// `DjogiField::set` / `set_expr` above and the rest of the wrapper
// methods on this type.
impl<M: Model, V> DjogiField<M, V> {
    /// Bind this target column to a source-tagged operand for an
    /// `INSERT INTO ... SELECT ...` statement — see
    /// [`FieldRef::copy_from`](crate::query::field::FieldRef::copy_from)
    /// for the full contract and the source/target identity guarantee.
    ///
    /// `V` must match between target and source — the type system pins
    /// the column types in lockstep at compile time. `S` is pinned by
    /// the source operand and propagated onto the returned column
    /// mapping; closure-return inference then ties `S` to the
    /// enclosing `QuerySet<S>::insert_into` receiver, so a mismatched
    /// source identity is rejected by the type system at the closure
    /// boundary.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use djogi::prelude::*;
    ///
    /// CompletedOrder::objects()
    ///     .filter(|f| f.completed_at().lt(cutoff))
    ///     .insert_into::<OrderArchive, _, _>(|target, source| vec![
    ///         target.original_id().copy_from(source.id().as_insert_source()),
    ///         target.title().copy_from(source.title().as_insert_source()),
    ///         target.completed_at().copy_from(source.completed_at().as_insert_source()),
    ///         target.status().copy_from(InsertSelectSource::literal("ARCHIVED".to_string())),
    ///     ])
    ///     .execute(&mut ctx)
    ///     .await?;
    /// ```
    #[must_use = "column mappings are lazy — drop one and the INSERT silently omits the column"]
    pub fn copy_from<S: Model>(
        self,
        source: crate::query::insert_select::InsertSelectSource<S, V>,
    ) -> crate::query::insert_select::InsertSelectColumn<S, M> {
        self.sql.copy_from(source)
    }

    /// Lift this source-side column reference into a tagged
    /// [`InsertSelectSource<M, V>`](crate::query::insert_select::InsertSelectSource)
    /// for use inside [`copy_from`](DjogiField::copy_from) on a
    /// target-side field.
    ///
    /// Mirrors [`FieldRef::as_insert_source`] — see that method for the
    /// full contract and the source/target identity guarantee.
    ///
    /// # Example
    ///
    /// ```ignore
    /// .insert_into::<OrderArchive, _, _>(|target, source| vec![
    ///     target.original_id().copy_from(source.id().as_insert_source()),
    /// ])
    /// ```
    #[must_use = "InsertSelectSource is lazy — drop one and the source projection is silently omitted"]
    pub fn as_insert_source(self) -> crate::query::insert_select::InsertSelectSource<M, V> {
        self.sql.as_insert_source()
    }
}

impl<M: Model, V> DjogiField<M, V> {
    /// `COUNT(column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn count(self) -> crate::expr::AggregateExpr<i64> {
        self.sql.count()
    }

    /// `COUNT(*)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn count_star(self) -> crate::expr::AggregateExpr<i64> {
        self.sql.count_star()
    }

    /// `ARRAY_AGG(column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn array_agg(self) -> crate::expr::AggregateExpr<Vec<V>> {
        self.sql.array_agg()
    }

    /// `JSONB_AGG(column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn json_agg(self) -> crate::expr::AggregateExpr<serde_json::Value> {
        self.sql.json_agg()
    }

    /// `JSON_OBJECT_AGG(key, value)` — see
    /// [`FieldRef::json_object_agg`](crate::query::FieldRef::json_object_agg).
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn json_object_agg<V2>(
        self,
        value: DjogiField<M, V2>,
    ) -> crate::expr::AggregateExpr<serde_json::Value> {
        self.sql.json_object_agg(value.sql)
    }

    /// `JSONB_OBJECT_AGG(key, value)` — see
    /// [`FieldRef::jsonb_object_agg`](crate::query::FieldRef::jsonb_object_agg).
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn jsonb_object_agg<V2>(
        self,
        value: DjogiField<M, V2>,
    ) -> crate::expr::AggregateExpr<serde_json::Value> {
        self.sql.jsonb_object_agg(value.sql)
    }

    /// `GROUPING(column)` — see
    /// [`FieldRef::grouping`](crate::query::FieldRef::grouping).
    /// Returns a metadata-kind aggregate: chaining modifier methods
    /// (`.distinct()` / `.filter(...)` / `.order_by(...)` / `.over(...)`)
    /// is a compile error because `AggregateExpr<i32, MetadataAgg>` does
    /// not implement those modifiers.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn grouping(self) -> crate::expr::AggregateExpr<i32, crate::expr::aggregate::MetadataAgg> {
        self.sql.grouping()
    }
}

impl<M: Model, V: crate::expr::arithmetic::Numeric> DjogiField<M, V> {
    /// `SUM(column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn sum(self) -> crate::expr::AggregateExpr<V> {
        self.sql.sum()
    }

    /// `AVG(column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn avg(self) -> crate::expr::AggregateExpr<f64> {
        self.sql.avg()
    }

    /// `STDDEV_POP(column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn stddev_pop(self) -> crate::expr::AggregateExpr<f64> {
        self.sql.stddev_pop()
    }

    /// `STDDEV_SAMP(column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn stddev_samp(self) -> crate::expr::AggregateExpr<f64> {
        self.sql.stddev_samp()
    }

    /// `STDDEV(column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn stddev(self) -> crate::expr::AggregateExpr<f64> {
        self.sql.stddev()
    }

    /// `VARIANCE(column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn variance(self) -> crate::expr::AggregateExpr<f64> {
        self.sql.variance()
    }

    /// `VAR_POP(column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn var_pop(self) -> crate::expr::AggregateExpr<f64> {
        self.sql.var_pop()
    }

    /// `VAR_SAMP(column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn var_samp(self) -> crate::expr::AggregateExpr<f64> {
        self.sql.var_samp()
    }

    /// `COVAR_POP(y, x)` — see
    /// [`FieldRef::covar_pop`](crate::query::FieldRef::covar_pop).
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn covar_pop<V2: crate::expr::arithmetic::Numeric>(
        self,
        x: DjogiField<M, V2>,
    ) -> crate::expr::AggregateExpr<f64> {
        self.sql.covar_pop(x.sql)
    }

    /// `COVAR_SAMP(y, x)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn covar_samp<V2: crate::expr::arithmetic::Numeric>(
        self,
        x: DjogiField<M, V2>,
    ) -> crate::expr::AggregateExpr<f64> {
        self.sql.covar_samp(x.sql)
    }

    /// `CORR(y, x)` — Pearson correlation coefficient.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn corr<V2: crate::expr::arithmetic::Numeric>(
        self,
        x: DjogiField<M, V2>,
    ) -> crate::expr::AggregateExpr<f64> {
        self.sql.corr(x.sql)
    }

    /// `REGR_AVGX(y, x)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn regr_avgx<V2: crate::expr::arithmetic::Numeric>(
        self,
        x: DjogiField<M, V2>,
    ) -> crate::expr::AggregateExpr<f64> {
        self.sql.regr_avgx(x.sql)
    }

    /// `REGR_AVGY(y, x)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn regr_avgy<V2: crate::expr::arithmetic::Numeric>(
        self,
        x: DjogiField<M, V2>,
    ) -> crate::expr::AggregateExpr<f64> {
        self.sql.regr_avgy(x.sql)
    }

    /// `REGR_COUNT(y, x)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn regr_count<V2: crate::expr::arithmetic::Numeric>(
        self,
        x: DjogiField<M, V2>,
    ) -> crate::expr::AggregateExpr<i64> {
        self.sql.regr_count(x.sql)
    }

    /// `REGR_INTERCEPT(y, x)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn regr_intercept<V2: crate::expr::arithmetic::Numeric>(
        self,
        x: DjogiField<M, V2>,
    ) -> crate::expr::AggregateExpr<f64> {
        self.sql.regr_intercept(x.sql)
    }

    /// `REGR_R2(y, x)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn regr_r2<V2: crate::expr::arithmetic::Numeric>(
        self,
        x: DjogiField<M, V2>,
    ) -> crate::expr::AggregateExpr<f64> {
        self.sql.regr_r2(x.sql)
    }

    /// `REGR_SLOPE(y, x)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn regr_slope<V2: crate::expr::arithmetic::Numeric>(
        self,
        x: DjogiField<M, V2>,
    ) -> crate::expr::AggregateExpr<f64> {
        self.sql.regr_slope(x.sql)
    }

    /// `REGR_SXX(y, x)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn regr_sxx<V2: crate::expr::arithmetic::Numeric>(
        self,
        x: DjogiField<M, V2>,
    ) -> crate::expr::AggregateExpr<f64> {
        self.sql.regr_sxx(x.sql)
    }

    /// `REGR_SXY(y, x)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn regr_sxy<V2: crate::expr::arithmetic::Numeric>(
        self,
        x: DjogiField<M, V2>,
    ) -> crate::expr::AggregateExpr<f64> {
        self.sql.regr_sxy(x.sql)
    }

    /// `REGR_SYY(y, x)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn regr_syy<V2: crate::expr::arithmetic::Numeric>(
        self,
        x: DjogiField<M, V2>,
    ) -> crate::expr::AggregateExpr<f64> {
        self.sql.regr_syy(x.sql)
    }

    /// `PERCENTILE_CONT(p) WITHIN GROUP (ORDER BY column)` — continuous
    /// percentile. Returns an ordered-set-kind aggregate; chaining
    /// `.distinct()` / `.over(...)` / `.order_by(...)` is a compile
    /// error because only [`OrderedSetAgg`](crate::expr::OrderedSetAgg)
    /// modifiers (`.filter(...)`, `.within_group_order_by(...)`) are
    /// legal.
    ///
    /// See [`FieldRef::percentile_cont`](crate::query::FieldRef::percentile_cont)
    /// for the full surface.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn percentile_cont(
        self,
        p: f64,
    ) -> crate::expr::AggregateExpr<f64, crate::expr::aggregate::OrderedSetAgg> {
        self.sql.percentile_cont(p)
    }
}

impl<M: Model, V: IntoFilterValue> DjogiField<M, V> {
    /// `MIN(column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn min(self) -> crate::expr::AggregateExpr<V> {
        self.sql.min()
    }

    /// `MAX(column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn max(self) -> crate::expr::AggregateExpr<V> {
        self.sql.max()
    }

    /// `PERCENTILE_DISC(p) WITHIN GROUP (ORDER BY column)` — discrete
    /// percentile. Returns an ordered-set-kind aggregate.
    ///
    /// See [`FieldRef::percentile_disc`](crate::query::FieldRef::percentile_disc).
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn percentile_disc(
        self,
        p: f64,
    ) -> crate::expr::AggregateExpr<V, crate::expr::aggregate::OrderedSetAgg> {
        self.sql.percentile_disc(p)
    }

    /// `MODE() WITHIN GROUP (ORDER BY column)` — most common value.
    /// Returns an ordered-set-kind aggregate.
    ///
    /// See [`FieldRef::mode`](crate::query::FieldRef::mode).
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn mode(self) -> crate::expr::AggregateExpr<V, crate::expr::aggregate::OrderedSetAgg> {
        self.sql.mode()
    }

    /// `RANK(value) WITHIN GROUP (ORDER BY column)` — hypothetical-set
    /// rank. Returns a hypothetical-set-kind aggregate; chaining
    /// `.distinct()` / `.over(...)` / `.order_by(...)` is a compile
    /// error because only [`HypotheticalSetAgg`](crate::expr::HypotheticalSetAgg)
    /// modifiers (`.filter(...)`, `.within_group_order_by(...)`) are
    /// legal.
    ///
    /// See [`FieldRef::rank_of`](crate::query::FieldRef::rank_of).
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn rank_of(
        self,
        value: V,
    ) -> crate::expr::AggregateExpr<i64, crate::expr::aggregate::HypotheticalSetAgg> {
        self.sql.rank_of(value)
    }

    /// `DENSE_RANK(value) WITHIN GROUP (ORDER BY column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn dense_rank_of(
        self,
        value: V,
    ) -> crate::expr::AggregateExpr<i64, crate::expr::aggregate::HypotheticalSetAgg> {
        self.sql.dense_rank_of(value)
    }

    /// `PERCENT_RANK(value) WITHIN GROUP (ORDER BY column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn percent_rank_of(
        self,
        value: V,
    ) -> crate::expr::AggregateExpr<f64, crate::expr::aggregate::HypotheticalSetAgg> {
        self.sql.percent_rank_of(value)
    }

    /// `CUME_DIST(value) WITHIN GROUP (ORDER BY column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn cume_dist_of(
        self,
        value: V,
    ) -> crate::expr::AggregateExpr<f64, crate::expr::aggregate::HypotheticalSetAgg> {
        self.sql.cume_dist_of(value)
    }
}

// Bitwise integer aggregates — same `IntegerColumn` seal as the
// `FieldRef` side: `i16` / `i32` / `i64`, never floats.
impl<M: Model, V: crate::expr::aggregate::IntegerColumn> DjogiField<M, V> {
    /// `BIT_AND(column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn bit_and(self) -> crate::expr::AggregateExpr<V> {
        self.sql.bit_and()
    }

    /// `BIT_OR(column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn bit_or(self) -> crate::expr::AggregateExpr<V> {
        self.sql.bit_or()
    }

    /// `BIT_XOR(column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn bit_xor(self) -> crate::expr::AggregateExpr<V> {
        self.sql.bit_xor()
    }
}

// ── DjogiField — equality and membership predicates ────────────────────────
//
// Bounds match the Sassi `Field<T, V>::eq/neq/in_/not_in` impls plus the
// Djogi SQL bind requirement, but only for types explicitly opted into
// `DjogiPortableEq`. SQL equality for some Postgres-native values can differ
// from Rust structural equality; `Interval` is the current typed-surface
// example and gets SQL-only methods below.

impl<M: Model, V> DjogiField<M, V>
where
    V: DjogiPortableEq,
{
    /// `column = value`. Portable: evaluates in Punnu and emits SQL.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn eq<P>(self, value: P) -> PortablePredicate<M>
    where
        P: IntoPortableFieldValue<V>,
    {
        let inner = self.portable.eq(value.into_portable_field_value());
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// `column <> value`. Portable: evaluates in Punnu and emits SQL.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn neq<P>(self, value: P) -> PortablePredicate<M>
    where
        P: IntoPortableFieldValue<V>,
    {
        let inner = self.portable.neq(value.into_portable_field_value());
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// `column IN (v1, …)`. Portable.
    ///
    /// An empty list lowers to SQL `FALSE` and Punnu `false`. Generic over
    /// any `IntoIterator<Item = V>` so callers can pass `Vec<V>`,
    /// `&[V]::iter().copied()`, or a custom range without preallocating.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn in_<I, P>(self, values: I) -> PortablePredicate<M>
    where
        I: IntoIterator<Item = P>,
        P: IntoPortableFieldValue<V>,
    {
        let inner = self.portable.in_(
            values
                .into_iter()
                .map(P::into_portable_field_value)
                .collect(),
        );
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// `column NOT IN (v1, …)`. Portable.
    ///
    /// An empty list lowers to SQL `TRUE` and Punnu `true`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn not_in<I, P>(self, values: I) -> PortablePredicate<M>
    where
        I: IntoIterator<Item = P>,
        P: IntoPortableFieldValue<V>,
    {
        let inner = self.portable.not_in(
            values
                .into_iter()
                .map(P::into_portable_field_value)
                .collect(),
        );
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }
}

// ── DjogiField — SQL-only Interval equality/membership ────────────────────
//
// PostgreSQL `INTERVAL =` linearizes months as 30 days and days as 24 hours
// before comparing. `crate::Interval` deliberately keeps structural
// `PartialEq`, so these methods must remain valid SQL predicates without being
// advertised as Punnu-evaluable portable predicates.

impl<M: Model> DjogiField<M, crate::Interval> {
    /// `interval_column = value` using PostgreSQL `INTERVAL` equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn eq(self, value: crate::Interval) -> Condition {
        self.sql.eq(value)
    }

    /// `interval_column <> value` using PostgreSQL `INTERVAL` equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn neq(self, value: crate::Interval) -> Condition {
        self.sql.neq(value)
    }

    /// `interval_column IN (v1, ...)` using PostgreSQL `INTERVAL` equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn in_<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = crate::Interval>,
    {
        self.sql.in_list(values)
    }

    /// `interval_column NOT IN (v1, ...)` using PostgreSQL `INTERVAL` equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_in<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = crate::Interval>,
    {
        self.sql.not_in_list(values)
    }
}

impl<M: Model> DjogiField<M, Option<crate::Interval>> {
    /// Nullable `INTERVAL` equality using PostgreSQL `INTERVAL` equality.
    ///
    /// `NULL` rows follow SQL three-valued logic. Use `is_null()` for an
    /// explicit NULL predicate.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn eq(self, value: crate::Interval) -> Condition {
        self.sql.eq(value)
    }

    /// Nullable `INTERVAL` inequality using PostgreSQL `INTERVAL` equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn neq(self, value: crate::Interval) -> Condition {
        self.sql.neq(value)
    }

    /// Nullable `INTERVAL IN (...)` using PostgreSQL `INTERVAL` equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn in_<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = crate::Interval>,
    {
        self.sql.in_list(values)
    }

    /// Nullable `INTERVAL NOT IN (...)` using PostgreSQL `INTERVAL` equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_in<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = crate::Interval>,
    {
        self.sql.not_in_list(values)
    }
}

// ── DjogiField — SQL-only network family (djogi#213) ──────────────────────
//
// `INET`, `CIDR`, and `MACADDR` columns route through SQL-only DjogiField
// methods for the same reason `Interval` does (see the bound-trait comment
// above): the proc macro classifier in `djogi-macros` is feature-blind, so
// classifying network types as portable scalars would cascade into
// `where V: DjogiPortableEq` bounds at the model expansion site that fail
// when the `network` feature is off. SQL-only impls keep the expansion
// working regardless of feature state and reach the typed network surface
// only when the feature is enabled (because that's when `IntoFilterValue`
// is also available).

#[cfg(feature = "network")]
impl<M: Model> DjogiField<M, std::net::IpAddr> {
    /// `inet_column = value` using PostgreSQL `INET` equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn eq(self, value: std::net::IpAddr) -> Condition {
        self.sql.eq(value)
    }

    /// `inet_column <> value` using PostgreSQL `INET` equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn neq(self, value: std::net::IpAddr) -> Condition {
        self.sql.neq(value)
    }

    /// `inet_column IN (v1, ...)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn in_<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = std::net::IpAddr>,
    {
        self.sql.in_list(values)
    }

    /// `inet_column NOT IN (v1, ...)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_in<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = std::net::IpAddr>,
    {
        self.sql.not_in_list(values)
    }
}

#[cfg(feature = "network")]
impl<M: Model> DjogiField<M, Option<std::net::IpAddr>> {
    /// Nullable `INET` equality. NULL rows follow SQL three-valued
    /// logic; use [`DjogiField::is_null`] for an explicit NULL test.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn eq(self, value: std::net::IpAddr) -> Condition {
        self.sql.eq(value)
    }

    /// Nullable `INET` inequality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn neq(self, value: std::net::IpAddr) -> Condition {
        self.sql.neq(value)
    }

    /// Nullable `INET IN (...)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn in_<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = std::net::IpAddr>,
    {
        self.sql.in_list(values)
    }

    /// Nullable `INET NOT IN (...)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_in<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = std::net::IpAddr>,
    {
        self.sql.not_in_list(values)
    }
}

#[cfg(feature = "network")]
impl<M: Model> DjogiField<M, crate::CidrAddr> {
    /// `cidr_column = value` using PostgreSQL `CIDR` equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn eq(self, value: crate::CidrAddr) -> Condition {
        self.sql.eq(value)
    }

    /// `cidr_column <> value` using PostgreSQL `CIDR` equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn neq(self, value: crate::CidrAddr) -> Condition {
        self.sql.neq(value)
    }

    /// `cidr_column IN (v1, ...)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn in_<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = crate::CidrAddr>,
    {
        self.sql.in_list(values)
    }

    /// `cidr_column NOT IN (v1, ...)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_in<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = crate::CidrAddr>,
    {
        self.sql.not_in_list(values)
    }
}

#[cfg(feature = "network")]
impl<M: Model> DjogiField<M, Option<crate::CidrAddr>> {
    /// Nullable `CIDR` equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn eq(self, value: crate::CidrAddr) -> Condition {
        self.sql.eq(value)
    }

    /// Nullable `CIDR` inequality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn neq(self, value: crate::CidrAddr) -> Condition {
        self.sql.neq(value)
    }

    /// Nullable `CIDR IN (...)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn in_<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = crate::CidrAddr>,
    {
        self.sql.in_list(values)
    }

    /// Nullable `CIDR NOT IN (...)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_in<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = crate::CidrAddr>,
    {
        self.sql.not_in_list(values)
    }
}

#[cfg(feature = "network")]
impl<M: Model> DjogiField<M, crate::MacAddr> {
    /// `macaddr_column = value` using PostgreSQL `MACADDR` equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn eq(self, value: crate::MacAddr) -> Condition {
        self.sql.eq(value)
    }

    /// `macaddr_column <> value` using PostgreSQL `MACADDR` equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn neq(self, value: crate::MacAddr) -> Condition {
        self.sql.neq(value)
    }

    /// `macaddr_column IN (v1, ...)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn in_<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = crate::MacAddr>,
    {
        self.sql.in_list(values)
    }

    /// `macaddr_column NOT IN (v1, ...)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_in<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = crate::MacAddr>,
    {
        self.sql.not_in_list(values)
    }
}

#[cfg(feature = "network")]
impl<M: Model> DjogiField<M, Option<crate::MacAddr>> {
    /// Nullable `MACADDR` equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn eq(self, value: crate::MacAddr) -> Condition {
        self.sql.eq(value)
    }

    /// Nullable `MACADDR` inequality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn neq(self, value: crate::MacAddr) -> Condition {
        self.sql.neq(value)
    }

    /// Nullable `MACADDR IN (...)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn in_<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = crate::MacAddr>,
    {
        self.sql.in_list(values)
    }

    /// Nullable `MACADDR NOT IN (...)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_in<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = crate::MacAddr>,
    {
        self.sql.not_in_list(values)
    }
}

// ── DjogiPresentField — present-only nullable network family ─────────────

#[cfg(feature = "network")]
impl<M: Model> DjogiPresentField<M, std::net::IpAddr> {
    /// Present-only nullable `INET = value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn eq(self, value: std::net::IpAddr) -> Condition {
        self.sql.eq(value)
    }

    /// Present-only nullable `INET <> value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn neq(self, value: std::net::IpAddr) -> Condition {
        self.sql.neq(value)
    }

    /// Present-only nullable `INET IN (...)`. An empty values list with
    /// a non-empty IS NOT NULL guard renders as `IS NOT NULL`, matching
    /// the present-only `Interval` semantics.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn in_<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = std::net::IpAddr>,
    {
        self.sql.in_list(values)
    }

    /// Present-only nullable `INET NOT IN (...)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_in<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = std::net::IpAddr>,
    {
        let mut values = values.into_iter().peekable();
        if values.peek().is_none() {
            self.sql.is_not_null()
        } else {
            self.sql.not_in_list(values)
        }
    }
}

#[cfg(feature = "network")]
impl<M: Model> DjogiPresentField<M, crate::CidrAddr> {
    /// Present-only nullable `CIDR = value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn eq(self, value: crate::CidrAddr) -> Condition {
        self.sql.eq(value)
    }

    /// Present-only nullable `CIDR <> value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn neq(self, value: crate::CidrAddr) -> Condition {
        self.sql.neq(value)
    }

    /// Present-only nullable `CIDR IN (...)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn in_<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = crate::CidrAddr>,
    {
        self.sql.in_list(values)
    }

    /// Present-only nullable `CIDR NOT IN (...)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_in<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = crate::CidrAddr>,
    {
        let mut values = values.into_iter().peekable();
        if values.peek().is_none() {
            self.sql.is_not_null()
        } else {
            self.sql.not_in_list(values)
        }
    }
}

#[cfg(feature = "network")]
impl<M: Model> DjogiPresentField<M, crate::MacAddr> {
    /// Present-only nullable `MACADDR = value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn eq(self, value: crate::MacAddr) -> Condition {
        self.sql.eq(value)
    }

    /// Present-only nullable `MACADDR <> value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn neq(self, value: crate::MacAddr) -> Condition {
        self.sql.neq(value)
    }

    /// Present-only nullable `MACADDR IN (...)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn in_<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = crate::MacAddr>,
    {
        self.sql.in_list(values)
    }

    /// Present-only nullable `MACADDR NOT IN (...)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_in<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = crate::MacAddr>,
    {
        let mut values = values.into_iter().peekable();
        if values.peek().is_none() {
            self.sql.is_not_null()
        } else {
            self.sql.not_in_list(values)
        }
    }
}

// ── DjogiField — ordering predicates (DjogiPortableOrd opt-in) ────────────
//
// Ordering is exposed only on types that opted into `DjogiPortableOrd`. The
// trait is sealed by absence of a blanket impl + the explicit per-type list
// above, which is what keeps `Option<U>` and unsupported foreign scalars
// out. Adopters that need `String` / `f32` / `f64` ordering reach for
// `explicit_pg_predicate().gt(...)` until a future phase pins
// collation/NaN parity.

impl<M: Model, V> DjogiField<M, V>
where
    V: DjogiPortableOrd,
{
    /// `column > value`. Portable: requires `V: DjogiPortableOrd`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn gt<P>(self, value: P) -> PortablePredicate<M>
    where
        P: IntoPortableFieldValue<V>,
    {
        let inner = self.portable.gt(value.into_portable_field_value());
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// `column >= value`. Portable: requires `V: DjogiPortableOrd`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn gte<P>(self, value: P) -> PortablePredicate<M>
    where
        P: IntoPortableFieldValue<V>,
    {
        let inner = self.portable.gte(value.into_portable_field_value());
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// `column < value`. Portable: requires `V: DjogiPortableOrd`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn lt<P>(self, value: P) -> PortablePredicate<M>
    where
        P: IntoPortableFieldValue<V>,
    {
        let inner = self.portable.lt(value.into_portable_field_value());
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// `column <= value`. Portable: requires `V: DjogiPortableOrd`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn lte<P>(self, value: P) -> PortablePredicate<M>
    where
        P: IntoPortableFieldValue<V>,
    {
        let inner = self.portable.lte(value.into_portable_field_value());
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// `column BETWEEN low AND high` (inclusive). Portable: requires
    /// `V: DjogiPortableOrd`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn between<P>(self, low: P, high: P) -> PortablePredicate<M>
    where
        P: IntoPortableFieldValue<V>,
    {
        let inner = self.portable.between(
            low.into_portable_field_value(),
            high.into_portable_field_value(),
        );
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }
}

// ── DjogiField — Option<U> predicates ──────────────────────────────────────
//
// Null tests apply on every `Option<U>` regardless of `U`'s bind/clone
// surface. `some()` returns the present-only view that exposes ordinary
// value comparisons. Direct ordering on `Option<U>` is **not** exposed —
// Rust `Option` ordering doesn't match SQL three-valued NULL semantics.

impl<M: Model, U: Send + Sync + 'static> DjogiField<M, Option<U>> {
    /// `column IS NULL`. Portable.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn is_null(self) -> PortablePredicate<M> {
        let inner = self.portable.is_null();
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// `column IS NOT NULL`. Portable.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn is_not_null(self) -> PortablePredicate<M> {
        let inner = self.portable.is_not_null();
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// Enter the present-only predicate view.
    ///
    /// `some().eq(v)` evaluates `Some(v)` and emits SQL `column = $1` —
    /// `None` evaluates to `false` in Punnu and SQL `NULL` excludes the
    /// row through three-valued logic.
    pub fn some(self) -> DjogiPresentField<M, U> {
        // Reuse the same column for the present-view's SQL handle. The
        // column was already validated at the parent `DjogiField`'s
        // construction site (`__make_djogi_field` runs `__make_field_ref`,
        // which calls `assert_plain_ident`); re-running validation here
        // would either be a no-op (for bare columns — the only shape
        // `__make_djogi_field` produces) or panic on the previously
        // interned path strings, so we reuse the validated string
        // directly through the crate-private `FieldRef::new`.
        DjogiPresentField {
            portable: self.portable.some(),
            sql: FieldRef::<M, U>::new(self.sql.column()),
        }
    }
}

// ── DjogiField<M, String> — portable string predicates ─────────────────────
//
// These mirror existing `FieldRef<M, String>` predicate names so adopter
// code keeps reading the same way. The SEMANTICS are pinned by the v3 plan:
// portable case-insensitive predicates use ASCII-stable folding (matches
// sassi PR1's `icontains`/`istarts_with`/`iends_with`/`iexact` evaluators),
// portable case-sensitive predicates spell their case-sensitivity
// explicitly. Database-locale Unicode folding (existing `FieldRef::contains`
// behaviour) lives only on `explicit_pg_predicate()`.

impl<M: Model> DjogiField<M, String> {
    /// Case-insensitive substring match (ASCII-stable). Portable.
    ///
    /// Use [`explicit_pg_predicate().contains`](ExplicitPgPredicateField::contains)
    /// for database-locale `ILIKE` semantics.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn contains(self, needle: &str) -> PortablePredicate<M> {
        let inner = self.portable.icontains(needle);
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// Alias for [`contains`](Self::contains) — Django naming parity.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn icontains(self, needle: &str) -> PortablePredicate<M> {
        self.contains(needle)
    }

    /// Case-insensitive prefix match (ASCII-stable). Portable.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn starts_with(self, prefix: &str) -> PortablePredicate<M> {
        let inner = self.portable.istarts_with(prefix);
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// Alias for [`starts_with`](Self::starts_with) — Django naming parity.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn istarts_with(self, prefix: &str) -> PortablePredicate<M> {
        self.starts_with(prefix)
    }

    /// Case-insensitive suffix match (ASCII-stable). Portable.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn ends_with(self, suffix: &str) -> PortablePredicate<M> {
        let inner = self.portable.iends_with(suffix);
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// Alias for [`ends_with`](Self::ends_with) — Django naming parity.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn iends_with(self, suffix: &str) -> PortablePredicate<M> {
        self.ends_with(suffix)
    }

    /// Case-sensitive substring match. Portable.
    ///
    /// New explicit name introduced by Phase 8eta — adopters opt into the
    /// case-sensitive shape rather than getting it implicitly. Sassi's
    /// `Field::contains` is case-sensitive; this method threads that
    /// through.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn contains_case_sensitive(self, needle: &str) -> PortablePredicate<M> {
        let inner = self.portable.contains(needle);
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// Case-sensitive prefix match. Portable.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn starts_with_case_sensitive(self, prefix: &str) -> PortablePredicate<M> {
        let inner = self.portable.starts_with(prefix);
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// Case-sensitive suffix match. Portable.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn ends_with_case_sensitive(self, suffix: &str) -> PortablePredicate<M> {
        let inner = self.portable.ends_with(suffix);
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// Case-insensitive equality (ASCII-stable). Portable.
    ///
    /// Maps to the Sassi `iexact` operator and lowers to a no-wildcard
    /// `COLLATE "C" ILIKE` comparison in SQL. Database-locale equality
    /// remains available through
    /// [`explicit_pg_predicate().iexact`](ExplicitPgPredicateField::iexact)
    /// once PR3 widens the explicit-PG surface.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn iexact(self, value: &str) -> PortablePredicate<M> {
        let inner = self.portable.iexact(value);
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// `STRING_AGG(column, sep)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn string_agg(self, sep: impl Into<String>) -> crate::expr::AggregateExpr<String> {
        self.sql.string_agg(sep)
    }

    /// Trigram similarity score expression — `similarity(col, $pattern)`.
    ///
    /// Returns an `Expr<f64>` that evaluates the `pg_trgm` `similarity()`
    /// function per row. Compose with the `Expr<T>` comparison API inside
    /// `filter_expr` to apply a per-query numeric threshold:
    ///
    /// ```ignore
    /// qs.filter_expr(|f| {
    ///     f.bio().trgm_similarity("query").gte(Expr::literal(0.3_f64))
    /// })
    /// ```
    ///
    /// **Index acceleration:** the function-form predicate emitted by
    /// `trgm_similarity(...).gte(...)` is **not** accelerated by
    /// `gin_trgm_ops` / `gist_trgm_ops` — those opclasses target the `%`
    /// operator family, not arbitrary `similarity(...)` >= comparisons.
    /// For index-accelerated trgm scans, use
    /// [`ExplicitPgPredicateField::trgm_similar_to`] via
    /// `f.col().explicit_pg_predicate().trgm_similar_to(pattern)` (the
    /// threshold for that path is the session GUC
    /// `pg_trgm.similarity_threshold`, default `0.3`).
    ///
    /// **Future work:** using `Expr<f64>` as an `order_by` target or as
    /// an `annotate` payload requires generic `Expr<T>` integration on
    /// `OrderExpr` and `AnnotationSlot`, which is not yet implemented.
    /// See `docs/guide/trgm.md` for the current limitations and the
    /// tracked follow-up.
    ///
    /// Requires the `pg_trgm` Postgres extension. Enable with
    /// `djogi = { features = ["trgm"] }`.
    ///
    /// See [`FieldRef::trgm_similarity`] for full documentation.
    #[cfg(feature = "trgm")]
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn trgm_similarity(self, pattern: impl Into<String>) -> crate::expr::Expr<f64> {
        self.sql.trgm_similarity(pattern)
    }
}

impl<M: Model> DjogiField<M, bool> {
    /// `BOOL_AND(column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn bool_and(self) -> crate::expr::AggregateExpr<bool> {
        self.sql.bool_and()
    }

    /// `BOOL_OR(column)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn bool_or(self) -> crate::expr::AggregateExpr<bool> {
        self.sql.bool_or()
    }

    /// `EVERY(column)` — Postgres-standard alias for `BOOL_AND`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn every(self) -> crate::expr::AggregateExpr<bool> {
        self.sql.every()
    }
}

// ── DjogiPresentField — present-only predicates ────────────────────────────
//
// Mirrors the Sassi `PresentField<T, V>` surface. Every method evaluates
// `None` as `false` in Punnu and emits SQL that excludes NULL rows through
// three-valued logic.

impl<M: Model, U> DjogiPresentField<M, U>
where
    U: DjogiPortableEq,
{
    /// `column IS NOT NULL AND column = value`. Portable.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn eq<P>(self, value: P) -> PortablePredicate<M>
    where
        P: IntoPortableFieldValue<U>,
    {
        let inner = self.portable.eq(value.into_portable_field_value());
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// `column IS NOT NULL AND column <> value`. Portable.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn neq<P>(self, value: P) -> PortablePredicate<M>
    where
        P: IntoPortableFieldValue<U>,
    {
        let inner = self.portable.neq(value.into_portable_field_value());
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// `column IS NOT NULL AND column IN (v1, …)`. Portable.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn in_<I, P>(self, values: I) -> PortablePredicate<M>
    where
        I: IntoIterator<Item = P>,
        P: IntoPortableFieldValue<U>,
    {
        let inner = self.portable.in_(
            values
                .into_iter()
                .map(P::into_portable_field_value)
                .collect(),
        );
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// `column IS NOT NULL AND column NOT IN (v1, …)`. Portable.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn not_in<I, P>(self, values: I) -> PortablePredicate<M>
    where
        I: IntoIterator<Item = P>,
        P: IntoPortableFieldValue<U>,
    {
        let inner = self.portable.not_in(
            values
                .into_iter()
                .map(P::into_portable_field_value)
                .collect(),
        );
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }
}

impl<M: Model> DjogiPresentField<M, crate::Interval> {
    /// Present-only nullable `INTERVAL = value` using PostgreSQL equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn eq(self, value: crate::Interval) -> Condition {
        self.sql.eq(value)
    }

    /// Present-only nullable `INTERVAL <> value` using PostgreSQL equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn neq(self, value: crate::Interval) -> Condition {
        self.sql.neq(value)
    }

    /// Present-only nullable `INTERVAL IN (...)` using PostgreSQL equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn in_<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = crate::Interval>,
    {
        self.sql.in_list(values)
    }

    /// Present-only nullable `INTERVAL NOT IN (...)` using PostgreSQL equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_in<I>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = crate::Interval>,
    {
        let mut values = values.into_iter().peekable();
        if values.peek().is_none() {
            self.sql.is_not_null()
        } else {
            self.sql.not_in_list(values)
        }
    }
}

impl<M: Model, U> DjogiPresentField<M, U>
where
    U: DjogiPortableOrd,
{
    /// `column IS NOT NULL AND column > value`. Portable.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn gt<P>(self, value: P) -> PortablePredicate<M>
    where
        P: IntoPortableFieldValue<U>,
    {
        let inner = self.portable.gt(value.into_portable_field_value());
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// `column IS NOT NULL AND column >= value`. Portable.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn gte<P>(self, value: P) -> PortablePredicate<M>
    where
        P: IntoPortableFieldValue<U>,
    {
        let inner = self.portable.gte(value.into_portable_field_value());
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// `column IS NOT NULL AND column < value`. Portable.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn lt<P>(self, value: P) -> PortablePredicate<M>
    where
        P: IntoPortableFieldValue<U>,
    {
        let inner = self.portable.lt(value.into_portable_field_value());
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// `column IS NOT NULL AND column <= value`. Portable.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn lte<P>(self, value: P) -> PortablePredicate<M>
    where
        P: IntoPortableFieldValue<U>,
    {
        let inner = self.portable.lte(value.into_portable_field_value());
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }

    /// `column IS NOT NULL AND low <= column <= high`. Portable.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn between<P>(self, low: P, high: P) -> PortablePredicate<M>
    where
        P: IntoPortableFieldValue<U>,
    {
        let inner = self.portable.between(
            low.into_portable_field_value(),
            high.into_portable_field_value(),
        );
        PortablePredicate::from_djogi_field(inner, DjogiFieldProvenance::mint_provenance())
    }
}

impl<M: Model, U> DjogiField<M, Option<U>>
where
    U: DjogiPortableOrd,
{
    /// `column > value` for a nullable scalar column. `NULL` rows are excluded.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn gt<P>(self, value: P) -> PortablePredicate<M>
    where
        P: IntoPortableFieldValue<U>,
    {
        self.some().gt(value)
    }

    /// `column >= value` for a nullable scalar column. `NULL` rows are excluded.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn gte<P>(self, value: P) -> PortablePredicate<M>
    where
        P: IntoPortableFieldValue<U>,
    {
        self.some().gte(value)
    }

    /// `column < value` for a nullable scalar column. `NULL` rows are excluded.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn lt<P>(self, value: P) -> PortablePredicate<M>
    where
        P: IntoPortableFieldValue<U>,
    {
        self.some().lt(value)
    }

    /// `column <= value` for a nullable scalar column. `NULL` rows are excluded.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn lte<P>(self, value: P) -> PortablePredicate<M>
    where
        P: IntoPortableFieldValue<U>,
    {
        self.some().lte(value)
    }

    /// `column BETWEEN low AND high` for a nullable scalar column.
    /// `NULL` rows are excluded.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn between<P>(self, low: P, high: P) -> PortablePredicate<M>
    where
        P: IntoPortableFieldValue<U>,
    {
        self.some().between(low, high)
    }
}

// ── ExplicitPgPredicateField — PostgreSQL-specific surface ─────────────────
//
// PR2a forwards the existing `FieldRef` PostgreSQL-specific predicate
// methods. Coverage matches what `FieldRef` already exposes for the
// receiver type — the goal in PR2a is API completeness so PR3 can flip the
// macro emission without leaving callers stranded. PR3 widens or trims as
// the macro flip surfaces additional methods.

impl<M: Model, V> ExplicitPgPredicateField<M, V> {
    /// Crate-private column-name accessor.
    ///
    /// Consumed by the MirJzSON SQL-only entry point
    /// (`query::mirjzson::ExplicitPgPredicateField<M, MirJzSON>::mirjzson`)
    /// when constructing `MirJzSONFieldRef<M>` — the future
    /// PostgreSQL-only operator surface needs the column name on hand
    /// to emit `Condition::MirJzSON(_)` shapes that bind the column
    /// reference. Public users do not consume this accessor; the
    /// adopter-facing predicate methods on `ExplicitPgPredicateField`
    /// route through `self.sql.<method>(value)` directly.
    #[doc(hidden)]
    pub(crate) fn __column(self) -> &'static str {
        self.sql.column()
    }

    /// `column = value` — equality through database-locale comparison
    /// rules. Forwarded from [`FieldRef::eq`].
    ///
    /// In PR2a this returns the same SQL shape as the portable `eq`, but
    /// keeping the route explicit lets PR3 preserve the cache-invalid
    /// rejection path for adopters who reach for this method
    /// deliberately.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn eq<P>(self, value: P) -> Condition
    where
        P: IntoFieldFilterValue<V>,
    {
        self.sql.eq(value)
    }

    /// `column <> value` — forwarded from [`FieldRef::neq`].
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn neq<P>(self, value: P) -> Condition
    where
        P: IntoFieldFilterValue<V>,
    {
        self.sql.neq(value)
    }

    /// `column > value` — forwarded from [`FieldRef::gt`]. Database-locale
    /// ordering.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn gt<P>(self, value: P) -> Condition
    where
        P: IntoFieldFilterValue<V>,
    {
        self.sql.gt(value)
    }

    /// `column >= value` — forwarded from [`FieldRef::gte`].
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn gte<P>(self, value: P) -> Condition
    where
        P: IntoFieldFilterValue<V>,
    {
        self.sql.gte(value)
    }

    /// `column < value` — forwarded from [`FieldRef::lt`].
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn lt<P>(self, value: P) -> Condition
    where
        P: IntoFieldFilterValue<V>,
    {
        self.sql.lt(value)
    }

    /// `column <= value` — forwarded from [`FieldRef::lte`].
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn lte<P>(self, value: P) -> Condition
    where
        P: IntoFieldFilterValue<V>,
    {
        self.sql.lte(value)
    }

    /// `column BETWEEN low AND high` — forwarded from
    /// [`FieldRef::between`].
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn between<P>(self, low: P, high: P) -> Condition
    where
        P: IntoFieldFilterValue<V>,
    {
        self.sql.between(low, high)
    }

    /// Case-insensitive equality through database-locale `LOWER(...)`.
    /// Forwarded from [`FieldRef::iexact`]. Distinct from the portable
    /// ASCII-stable `DjogiField::iexact`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn iexact<P>(self, value: P) -> Condition
    where
        P: IntoFieldFilterValue<V>,
    {
        self.sql.iexact(value)
    }
}

impl<M: Model, V> ExplicitPgPredicateField<M, V> {
    /// `column IN (v1, …)`. Forwarded from [`FieldRef::in_list`].
    ///
    /// Named `in_list` to match the `FieldRef` naming convention rather
    /// than `in_` — the explicit-PG view is intentionally close to the
    /// existing SQL surface.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn in_list<I, P>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = P>,
        P: IntoFieldFilterValue<V>,
    {
        self.sql.in_list(values)
    }

    /// `column NOT IN (v1, …)`. Forwarded from [`FieldRef::not_in_list`].
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_in_list<I, P>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = P>,
        P: IntoFieldFilterValue<V>,
    {
        self.sql.not_in_list(values)
    }
}

impl<M: Model, V> ExplicitPgPredicateField<M, V> {
    /// `column IS NULL` — forwarded from [`FieldRef::is_null`].
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn is_null(self) -> Condition {
        self.sql.is_null()
    }

    /// `column IS NOT NULL` — forwarded from [`FieldRef::is_not_null`].
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn is_not_null(self) -> Condition {
        self.sql.is_not_null()
    }
}

// String-only PostgreSQL-specific surface. Mirrors the existing
// `FieldRef<M, String>` block: database-locale `ILIKE` family + Postgres
// POSIX regex.

impl<M: Model> ExplicitPgPredicateField<M, String> {
    /// Case-insensitive substring match through Postgres `ILIKE` and the
    /// database's text collation. Forwarded from [`FieldRef::contains`].
    ///
    /// Distinct from `DjogiField::contains` (ASCII-stable, portable).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn contains(self, value: impl Into<String>) -> Condition {
        self.sql.contains(value)
    }

    /// Alias for [`contains`](Self::contains).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn icontains(self, value: impl Into<String>) -> Condition {
        self.sql.icontains(value)
    }

    /// Case-insensitive prefix match through Postgres `ILIKE`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn starts_with(self, value: impl Into<String>) -> Condition {
        self.sql.starts_with(value)
    }

    /// Alias for [`starts_with`](Self::starts_with).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn istarts_with(self, value: impl Into<String>) -> Condition {
        self.sql.istarts_with(value)
    }

    /// Case-insensitive suffix match through Postgres `ILIKE`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn ends_with(self, value: impl Into<String>) -> Condition {
        self.sql.ends_with(value)
    }

    /// Alias for [`ends_with`](Self::ends_with).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn iends_with(self, value: impl Into<String>) -> Condition {
        self.sql.iends_with(value)
    }

    /// Postgres POSIX regex match — `column ~ $1`. Forwarded from
    /// [`FieldRef::regex`]. Server-side; no Rust regex engine is involved.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn regex(self, value: impl Into<String>) -> Condition {
        self.sql.regex(value)
    }

    /// Postgres POSIX regex match (case-insensitive) — `column ~* $1`.
    /// Forwarded from [`FieldRef::iregex`].
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn iregex(self, value: impl Into<String>) -> Condition {
        self.sql.iregex(value)
    }

    /// Trigram similarity predicate — `<col> % $pattern`.
    ///
    /// Compiles to the `%` operator, which is the indexable strategy
    /// member of the `gin_trgm_ops` and `gist_trgm_ops` opclasses — a
    /// GIN or GiST index built with one of those opclasses accelerates
    /// the predicate.
    ///
    /// **Threshold:** the threshold for `%` is the session GUC
    /// `pg_trgm.similarity_threshold` (Postgres default `0.3`). For a
    /// per-query numeric threshold use
    /// [`DjogiField::trgm_similarity`] inside `filter_expr` — that form
    /// is NOT index-accelerated by the trgm opclasses.
    ///
    /// The pattern is a positional bind parameter — no user text is
    /// interpolated into SQL.
    ///
    /// Requires the `pg_trgm` Postgres extension. Enable with
    /// `djogi = { features = ["trgm"] }`.
    ///
    /// See [`FieldRef::trgm_similar_to`] for full documentation.
    #[cfg(feature = "trgm")]
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn trgm_similar_to(self, pattern: impl Into<String>) -> Condition {
        self.sql.trgm_similar_to(pattern)
    }
}

impl<M: Model, V: IntoArrayFilterValue + Clone + 'static> ExplicitPgPredicateField<M, Vec<V>> {
    /// Postgres array contains (`@>`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn contains(self, values: &[V]) -> Condition {
        self.sql.contains(values)
    }

    /// Postgres array contained-by (`<@`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn contained_by(self, values: &[V]) -> Condition {
        self.sql.contained_by(values)
    }

    /// Postgres array overlap (`&&`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn overlap(self, values: &[V]) -> Condition {
        self.sql.overlap(values)
    }
}

impl<M: Model, T> ExplicitPgPredicateField<M, crate::Range<T>>
where
    T: crate::range::RangeElement + IntoFilterValue,
{
    /// Postgres range contains element (`@>`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn contains(self, value: T) -> Condition {
        self.sql.contains(value)
    }

    /// Postgres range contains range (`@>`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn contains_range(self, range: crate::Range<T>) -> Condition {
        self.sql.contains_range(range)
    }

    /// Postgres range contained-by (`<@`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn contained_by(self, range: crate::Range<T>) -> Condition {
        self.sql.contained_by(range)
    }

    /// Postgres range overlap (`&&`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn overlaps(self, range: crate::Range<T>) -> Condition {
        self.sql.overlaps(range)
    }

    /// Postgres range strictly-left (`<<`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn strictly_left_of(self, range: crate::Range<T>) -> Condition {
        self.sql.strictly_left_of(range)
    }

    /// Postgres range strictly-right (`>>`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn strictly_right_of(self, range: crate::Range<T>) -> Condition {
        self.sql.strictly_right_of(range)
    }

    /// Postgres range does-not-extend-right (`&<`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_extends_right_of(self, range: crate::Range<T>) -> Condition {
        self.sql.not_extends_right_of(range)
    }

    /// Postgres range does-not-extend-left (`&>`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_extends_left_of(self, range: crate::Range<T>) -> Condition {
        self.sql.not_extends_left_of(range)
    }

    /// Postgres range adjacency (`-|-`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn adjacent_to(self, range: crate::Range<T>) -> Condition {
        self.sql.adjacent_to(range)
    }
}

impl<M: Model, T> DjogiPresentField<M, crate::Range<T>>
where
    T: crate::range::RangeElement + IntoFilterValue,
{
    /// Present-only nullable range contains element (`@>`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn contains(self, value: T) -> Condition {
        self.sql.contains(value)
    }

    /// Present-only nullable range contains range (`@>`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn contains_range(self, range: crate::Range<T>) -> Condition {
        self.sql.contains_range(range)
    }

    /// Present-only nullable range contained-by (`<@`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn contained_by(self, range: crate::Range<T>) -> Condition {
        self.sql.contained_by(range)
    }

    /// Present-only nullable range overlap (`&&`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn overlaps(self, range: crate::Range<T>) -> Condition {
        self.sql.overlaps(range)
    }

    /// Present-only nullable range strictly-left (`<<`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn strictly_left_of(self, range: crate::Range<T>) -> Condition {
        self.sql.strictly_left_of(range)
    }

    /// Present-only nullable range strictly-right (`>>`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn strictly_right_of(self, range: crate::Range<T>) -> Condition {
        self.sql.strictly_right_of(range)
    }

    /// Present-only nullable range does-not-extend-right (`&<`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_extends_right_of(self, range: crate::Range<T>) -> Condition {
        self.sql.not_extends_right_of(range)
    }

    /// Present-only nullable range does-not-extend-left (`&>`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_extends_left_of(self, range: crate::Range<T>) -> Condition {
        self.sql.not_extends_left_of(range)
    }

    /// Present-only nullable range adjacency (`-|-`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn adjacent_to(self, range: crate::Range<T>) -> Condition {
        self.sql.adjacent_to(range)
    }
}

impl<M: Model, V: IntoArrayFilterValue + Clone + 'static> DjogiField<M, Vec<V>> {
    /// `array_length(column, 1)`.
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn len(self) -> crate::expr::Expr<i32> {
        self.sql.len()
    }

    /// Whether this array field has no elements.
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn is_empty(self) -> crate::expr::Expr<bool> {
        self.len().eq(0)
    }
}

impl<M: Model, T> ExplicitPgPredicateField<M, Jsonb<T>> {
    /// Navigate to a JSONB sub-path.
    #[must_use = "JsonbPathRef is lazy — dropping one silently omits the filter"]
    pub fn path<V>(self, dotted: &'static str) -> crate::jsonb::JsonbPathRef<M, V> {
        self.sql.path(dotted)
    }

    /// Enter the compile-time typed JSONB path tree.
    #[must_use = "typed path handles are lazy — dropping one silently omits the filter"]
    pub fn typed(self) -> T::Path<M>
    where
        T: crate::jsonb::JsonbSchema,
    {
        self.sql.typed()
    }
}

impl<M: Model, T> ExplicitPgPredicateField<M, Option<Jsonb<T>>> {
    /// Navigate to a JSONB sub-path on a nullable JSONB column.
    #[must_use = "JsonbPathRef is lazy — dropping one silently omits the filter"]
    pub fn path<V>(self, dotted: &'static str) -> crate::jsonb::JsonbPathRef<M, V> {
        self.sql.path(dotted)
    }

    /// Enter the compile-time typed JSONB path tree on a nullable JSONB column.
    #[must_use = "typed path handles are lazy — dropping one silently omits the filter"]
    pub fn typed(self) -> T::Path<M>
    where
        T: crate::jsonb::JsonbSchema,
    {
        self.sql.typed()
    }
}

#[cfg(feature = "spatial")]
impl<M: crate::model::Model> ExplicitPgPredicateField<M, crate::geo::GeoPoint> {
    /// PostGIS radius predicate (`ST_DWithin`).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn within_km(self, center: crate::geo::GeoPoint, km: f64) -> Condition {
        self.sql.within_km(center, km)
    }

    /// PostGIS distance expression (`ST_Distance`).
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn distance_to(self, center: &crate::geo::GeoPoint) -> crate::expr::Expr<f64> {
        self.sql.distance_to(center)
    }
}

#[cfg(feature = "spatial")]
impl<M: crate::model::Model> DjogiField<M, crate::geo::GeoPoint> {
    /// Order by distance from `center`.
    #[must_use = "order expressions are inert until passed to `order_by`"]
    pub fn order_by_distance(self, center: crate::geo::GeoPoint) -> crate::query::order::OrderExpr {
        self.sql.order_by_distance(center)
    }

    /// `ST_MakeLine` aggregate over point rows.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn make_line(self) -> crate::expr::AggregateExpr<crate::geo::LineString> {
        self.sql.make_line()
    }
}

#[cfg(feature = "spatial")]
impl<M: crate::model::Model> ExplicitPgPredicateField<M, Option<crate::geo::GeoPoint>> {
    /// PostGIS radius predicate on nullable points.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn within_km(self, center: crate::geo::GeoPoint, km: f64) -> Condition {
        self.sql.within_km(center, km)
    }
}

#[cfg(feature = "spatial")]
impl<M: crate::model::Model> DjogiField<M, Option<crate::geo::GeoPoint>> {
    /// Order by distance from `center`, with Postgres NULL ordering semantics.
    #[must_use = "order expressions are inert until passed to `order_by`"]
    pub fn order_by_distance(self, center: crate::geo::GeoPoint) -> crate::query::order::OrderExpr {
        self.sql.order_by_distance(center)
    }
}

#[cfg(feature = "spatial")]
impl<M: crate::model::Model, G: crate::geo::GeographyValue> ExplicitPgPredicateField<M, G> {
    /// PostGIS `ST_Contains` predicate.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn contains<O: crate::geo::GeographyValue>(self, other: &O) -> Condition {
        self.sql.contains(other)
    }

    /// PostGIS `ST_Intersects` predicate.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn intersects<O: crate::geo::GeographyValue>(self, other: &O) -> Condition {
        self.sql.intersects(other)
    }

    /// PostGIS `ST_Touches` predicate.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn touches<O: crate::geo::GeographyValue>(self, other: &O) -> Condition {
        self.sql.touches(other)
    }

    /// PostGIS shape `ST_Within` predicate.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn within<O: crate::geo::GeographyValue>(self, other: &O) -> Condition {
        self.sql.within(other)
    }

    /// PostGIS bounding-box expression predicate.
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn bounded_by(
        self,
        min_lat: f64,
        min_lon: f64,
        max_lat: f64,
        max_lon: f64,
    ) -> crate::expr::Expr<bool> {
        self.sql.bounded_by(min_lat, min_lon, max_lat, max_lon)
    }
}

#[cfg(feature = "spatial")]
impl<M: crate::model::Model, G: crate::geo::GeographyValue> DjogiField<M, G> {
    /// `ST_ConvexHull(ST_Collect(...))` aggregate.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn convex_hull(self) -> crate::expr::AggregateExpr<crate::geo::Polygon> {
        self.sql.convex_hull()
    }

    /// `ST_Centroid(ST_Collect(...))` aggregate.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn centroid(self) -> crate::expr::AggregateExpr<crate::geo::GeoPoint> {
        self.sql.centroid()
    }

    /// `ST_Collect(...)` aggregate.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn collect(self) -> crate::expr::AggregateExpr<crate::geo::MultiPoint> {
        self.sql.collect()
    }

    /// `ST_Extent(...)` aggregate.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn extent(self) -> crate::expr::AggregateExpr<crate::geo::Polygon> {
        self.sql.extent()
    }

    /// `ST_3DExtent(...)` aggregate.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn extent_3d(self) -> crate::expr::AggregateExpr<crate::geo::Polygon> {
        self.sql.extent_3d()
    }
}

#[cfg(feature = "spatial")]
impl<M: crate::model::Model> DjogiField<M, crate::geo::Polygon> {
    /// `ST_Union(...)` aggregate.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn union(self) -> crate::expr::AggregateExpr<crate::geo::MultiPolygon> {
        self.sql.union()
    }

    /// `ST_Collect(...)` polygon aggregate.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn polygon_agg(self) -> crate::expr::AggregateExpr<crate::geo::MultiPolygon> {
        self.sql.polygon_agg()
    }

    /// `ST_ClusterIntersecting(...)` aggregate.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn cluster_intersecting(self) -> crate::expr::AggregateExpr<Vec<crate::geo::MultiPolygon>> {
        self.sql.cluster_intersecting()
    }

    /// `ST_ClusterWithin(...)` aggregate.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn cluster_within(
        self,
        distance: f64,
    ) -> crate::expr::AggregateExpr<Vec<crate::geo::MultiPolygon>> {
        self.sql.cluster_within(distance)
    }

    /// `ST_MemUnion(...)` aggregate.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn mem_union(self) -> crate::expr::AggregateExpr<crate::geo::MultiPolygon> {
        self.sql.mem_union()
    }
}

#[cfg(feature = "spatial")]
impl<M: crate::model::Model> DjogiField<M, crate::geo::LineString> {
    /// `ST_Polygonize(...)` aggregate.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn polygonize(self) -> crate::expr::AggregateExpr<crate::geo::MultiPolygon> {
        self.sql.polygonize()
    }

    /// `ST_LineAgg(...)` aggregate.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn line_agg(self) -> crate::expr::AggregateExpr<crate::geo::MultiLineString> {
        self.sql.line_agg()
    }
}

#[cfg(feature = "spatial")]
impl<M: crate::model::Model> DjogiField<M, crate::geo::MultiPolygon> {
    /// `ST_Union(...)` aggregate.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn union(self) -> crate::expr::AggregateExpr<crate::geo::MultiPolygon> {
        self.sql.union()
    }

    /// `ST_MemUnion(...)` aggregate.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn mem_union(self) -> crate::expr::AggregateExpr<crate::geo::MultiPolygon> {
        self.sql.mem_union()
    }

    /// `ST_ClusterIntersecting(...)` aggregate.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn cluster_intersecting(self) -> crate::expr::AggregateExpr<Vec<crate::geo::MultiPolygon>> {
        self.sql.cluster_intersecting()
    }

    /// `ST_ClusterWithin(...)` aggregate.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn cluster_within(
        self,
        distance: f64,
    ) -> crate::expr::AggregateExpr<Vec<crate::geo::MultiPolygon>> {
        self.sql.cluster_within(distance)
    }
}

// ── IntoSqlField — sealed conversion to FieldRef for SQL helper APIs ─────────
//
// PR3 needs `FieldRef`-shaped SQL helper APIs (spatial group/cluster keys,
// aggregate column probes, etc.) to accept either of:
//
// - the legacy `FieldRef<M, V>` directly, or
// - the new generated root accessor return type `DjogiField<M, V>`.
//
// `IntoSqlField<M, V>` is the sealed bridge that adapts both into a
// `FieldRef<M, V>`. Sealing the trait at the module boundary keeps
// downstream crates from injecting their own column strings — the trait is
// nameable as a bound (so signatures like
// `pub fn group_by_region<F, R>(...) where F: FnOnce(T::Fields) -> impl
// IntoSqlField<T, G>` compile), but only `FieldRef` and `DjogiField` can
// satisfy it. Identifier-smuggling stays closed because the only
// implementations forward the validated column string from one of those
// two types.
//
// The trait deliberately does not accept tuples or other compound shapes:
// downstream APIs that need a fan-out (DISTINCT ON, GROUP BY, …) reach
// for `IntoDistinctColumns` / `IntoGroupKeyTuple` instead.

mod sql_field_seal {
    pub trait Sealed {}
}

/// Sealed conversion from a typed root field handle into a SQL `FieldRef`.
///
/// Implemented for:
///
/// - [`FieldRef<M, V>`] — legacy SQL-only handle. Returns the receiver
///   unchanged.
/// - [`DjogiField<M, V>`] — Phase 8eta root accessor return type. Returns
///   the wrapped SQL `FieldRef` so the SQL helper sees the same column
///   metadata it always has.
///
/// Use this trait as a bound on SQL helper signatures (spatial group/cluster
/// keys, single-column probes for aggregate emission helpers, etc.) so root
/// accessors can flow through without an explicit `.explicit_pg_predicate()`
/// /  `__sql_field()` step. The bound is sealed, so downstream crates cannot
/// add hostile impls that would smuggle arbitrary column strings into
/// SQL emission sites — the only way to reach a `FieldRef<M, V>` here is
/// through a value already produced by the validated `__make_field_ref` /
/// `__make_djogi_field` constructors.
///
/// The trait deliberately covers single columns only: helpers that fan
/// out into a column tuple (`DISTINCT ON`, `GROUP BY`) reach for
/// [`crate::query::queryset::IntoDistinctColumns`] /
/// [`crate::query::grouped::IntoGroupKeyTuple`] instead.
pub trait IntoSqlField<M: Model, V>: sql_field_seal::Sealed {
    /// Convert into a `FieldRef<M, V>` carrying the same column metadata.
    fn into_sql_field(self) -> FieldRef<M, V>;
}

impl<M: Model, V> sql_field_seal::Sealed for FieldRef<M, V> {}
impl<M: Model, V> IntoSqlField<M, V> for FieldRef<M, V> {
    fn into_sql_field(self) -> FieldRef<M, V> {
        self
    }
}

impl<M: Model, V> sql_field_seal::Sealed for DjogiField<M, V> {}
impl<M: Model, V> IntoSqlField<M, V> for DjogiField<M, V> {
    fn into_sql_field(self) -> FieldRef<M, V> {
        // Reuse the SQL handle the wrapper already owns — `__sql_field`
        // is crate-private but reachable from this module. The column
        // string was validated by `__make_djogi_field` at construction
        // (which routes through `__make_field_ref::<M, V>`); no
        // re-validation is required here.
        self.__sql_field()
    }
}

// ── Macro-construction support ─────────────────────────────────────────────
//
// `__make_djogi_field` is the single entry point macro-emitted code uses to
// stamp a `DjogiField<M, V>` for one column. PR3 routes every generated
// `{Model}Fields` accessor through this function, so the trusted-construction
// invariants flow through one validation gate.

#[doc(hidden)]
pub mod djogi_field_macro_support {
    //! Macro-only entry points for `DjogiField` construction.
    //!
    //! Same conventions as [`super::__macro_support`]: items are `pub` so
    //! cross-crate macro emission can reach them; the double-underscore
    //! prefix and `#[doc(hidden)]` marker signal that downstream code must
    //! not call them directly.

    use super::{__macro_support::__make_field_ref, DjogiField, FieldRef};
    use crate::model::Model;

    /// Construct a [`DjogiField<M, V>`] for one root column.
    ///
    /// `column` is the bare physical column name (no relation/path prefix
    /// — relation/visage traversal is SQL-only and uses a different
    /// constructor in PR3). `extract` is the `fn(&M) -> &V` pointer that
    /// the macro stamps from the model's struct definition.
    ///
    /// The function:
    ///
    /// - Validates `column` through the same identifier gate
    ///   [`__make_field_ref`] uses, rejecting reserved keywords / metadata
    ///   bytes / over-long names at construction time.
    /// - Constructs a `sassi::Field<M, V>` with the same `column` string
    ///   so portable predicates and SQL emission target the same column
    ///   name by construction.
    /// - Constructs a `FieldRef<M, V>` through the same intern path the
    ///   shipped macro already uses.
    ///
    /// **Function pointer, not closure.** `extract: fn(&M) -> &V` keeps
    /// `DjogiField` `Copy` without captured state; computed/projected
    /// values that need captured state are non-portable in 8eta and reach
    /// the database through `explicit_pg_predicate()` or a generated
    /// SQL-only handle.
    #[doc(hidden)]
    pub fn __make_djogi_field<M, V>(column: &'static str, extract: fn(&M) -> &V) -> DjogiField<M, V>
    where
        M: Model,
    {
        let sql: FieldRef<M, V> = __make_field_ref::<M, V>(None, column);
        let portable = ::sassi::Field::<M, V>::new(column, extract);
        DjogiField {
            portable,
            sql,
            extractor: extract,
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
///
/// # Two distinct responsibilities
///
/// 1. **Bind-value conversion** — [`into_filter_value`](Self::into_filter_value)
///    maps a Rust value to a typed [`FilterValue`] discriminant for the
///    SQL bind site. This is the universal direction: every implementor
///    must define it.
///
/// 2. **JSONB path LHS cast selection** —
///    [`jsonb_sql_cast`](Self::jsonb_sql_cast) returns the typed
///    [`JsonbSqlCast`](crate::jsonb::JsonbSqlCast) that
///    [`JsonbPathRef<M, Self>`](crate::jsonb::JsonbPathRef) applies to
///    the text-extracted LHS before comparing against the bind. The
///    default body walks a built-in lookup table keyed by
///    `std::any::type_name::<Self>()`; that covers every primitive Rust
///    type djogi ships an impl for. **Wrapper implementors (custom PK
///    newtypes from `primary_key!`, `ForeignKey<T>`, `OneToOneField<T>`,
///    and any other inner-type-delegating shape) MUST override
///    `jsonb_sql_cast` to delegate to the inner SQL value type's impl.**
///    Otherwise the default lookup misses the wrapper's `type_name` and
///    JSONB path comparisons silently fall back to text — `'10' < '9'`
///    because text ordering is lexicographic.
pub trait IntoFilterValue {
    /// Convert `self` into the typed [`FilterValue`] discriminant the
    /// SQL bind site accepts.
    fn into_filter_value(self) -> FilterValue;

    /// Return the typed Postgres cast applied to the JSONB path LHS
    /// when this type appears as the value generic of
    /// [`JsonbPathRef<M, Self>`](crate::jsonb::JsonbPathRef).
    ///
    /// The default body resolves the type's `std::any::type_name`
    /// through djogi's built-in cast table. This covers every primitive
    /// the framework ships an `IntoFilterValue` impl for (integers,
    /// floats, bool, temporal, UUID, Decimal, Interval, HeerId/RanjId
    /// family, INET/CIDR/MACADDR under the `network` feature).
    ///
    /// **Wrapper types MUST override this method** to delegate to their
    /// inner SQL value type's `jsonb_sql_cast` — otherwise the wrapper's
    /// own `type_name` falls through to `None` and JSONB path
    /// comparisons against the wrapper silently use text comparison.
    /// Examples in this crate:
    ///
    /// - [`primary_key!`](crate::primary_key!)-emitted custom PK newtypes
    ///   delegate to the inner Rust type the macro declared (e.g.
    ///   `LocalI64Id(i64)` delegates to `i64`).
    /// - [`ForeignKey<T>`](crate::ForeignKey) delegates to `T::Pk`.
    /// - [`OneToOneField<T>`](crate::OneToOneField) delegates to `T::Pk`.
    ///
    /// Returns `None` for `String` / `&str` (text extraction already
    /// produces text — no cast needed) and for any type not in the
    /// table. Adopters defining a custom scalar type for a JSONB path
    /// extend this trait by implementing `IntoFilterValue` and
    /// overriding `jsonb_sql_cast` to return the matching variant.
    fn jsonb_sql_cast() -> Option<crate::jsonb::JsonbSqlCast>
    where
        Self: Sized,
    {
        crate::jsonb::path::jsonb_sql_cast_for_type(std::any::type_name::<Self>())
    }
}

/// Convert a lookup argument into the SQL bind value for a specific field type.
///
/// `IntoFilterValue` answers "can this value become a SQL bind?" while this
/// trait answers "can this value be used against this field's declared type?".
/// That distinction lets `FieldRef<M, String>::eq("x")`,
/// `FieldRef<M, Option<i16>>::lte(1970_i16)`, and
/// `FieldRef<M, Tracked<String>>::eq("x")` compile without allowing
/// unrelated mismatches such as `FieldRef<M, i32>::eq("x")`.
///
/// # Shipped impls
///
/// - `impl<V: IntoFilterValue> IntoFieldFilterValue<V> for V` — the blanket
///   that keeps every existing `field.eq(v)` call site compiling (passing
///   the column's declared `V` directly).
/// - `IntoFieldFilterValue<String> for &str` — accept `&str` literals against
///   `String` columns (issue #167).
/// - `impl<V: IntoFilterValue> IntoFieldFilterValue<Option<V>> for V` and
///   `IntoFieldFilterValue<Option<String>> for &str` — pass the inner scalar
///   against a nullable column; NULL rows are excluded by SQL three-valued
///   logic at emission time (issue #107).
/// - `impl<V: IntoFilterValue> IntoFieldFilterValue<Tracked<V>> for V` and
///   `IntoFieldFilterValue<Tracked<String>> for &str` — pass the inner value
///   against a `Tracked<V>` column; dirty-flag state is irrelevant on the
///   SQL bind path (issue #166).
///
/// # Why not blanket `Into<FilterValue>`?
///
/// A bare `Into<FilterValue>` bound on `eq` would silently widen every typed
/// `FieldRef<M, V>::eq` call to accept any SQL-bindable value, dropping the
/// column-type check at the API boundary. The `FieldValue` type parameter
/// here keeps the mismatch error at the field-argument conversion bound
/// (`view_count.eq("hello")` fails at `&str: IntoFieldFilterValue<i64>`,
/// which is intentionally not impl'd).
pub trait IntoFieldFilterValue<FieldValue> {
    /// Convert `self` into the SQL bind value the field's lookup leaf carries.
    ///
    /// The returned [`FilterValue`] feeds the typed Postgres bind site in
    /// `query::sql`. Implementors must return a variant whose runtime type
    /// matches what the column's emitter expects (e.g. `FilterValue::String`
    /// for `String` / `Option<String>` / `Tracked<String>` columns).
    fn into_field_filter_value(self) -> FilterValue;
}

impl<V> IntoFieldFilterValue<V> for V
where
    V: IntoFilterValue,
{
    fn into_field_filter_value(self) -> FilterValue {
        self.into_filter_value()
    }
}

impl IntoFieldFilterValue<String> for &str {
    fn into_field_filter_value(self) -> FilterValue {
        FilterValue::String(self.to_owned())
    }
}

impl<V> IntoFieldFilterValue<Option<V>> for V
where
    V: IntoFilterValue,
{
    fn into_field_filter_value(self) -> FilterValue {
        self.into_filter_value()
    }
}

impl IntoFieldFilterValue<Option<String>> for &str {
    fn into_field_filter_value(self) -> FilterValue {
        FilterValue::String(self.to_owned())
    }
}

impl<V> IntoFieldFilterValue<Tracked<V>> for V
where
    V: IntoFilterValue,
{
    fn into_field_filter_value(self) -> FilterValue {
        self.into_filter_value()
    }
}

impl IntoFieldFilterValue<Tracked<String>> for &str {
    fn into_field_filter_value(self) -> FilterValue {
        FilterValue::String(self.to_owned())
    }
}

/// Convert an argument into the value type Sassi evaluates for portable fields.
///
/// Portable predicates need a real `V` for `sassi::Field<M, V>`, not just a
/// SQL bind. These impls keep the public lookup ergonomics aligned with the SQL
/// surface while preserving the field's declared type for in-memory predicate
/// evaluation. Mirrors [`IntoFieldFilterValue`] one impl at a time — the SQL
/// emitter and Punnu agree on which forms reach each field type.
///
/// # Shipped impls
///
/// - `impl<V> IntoPortableFieldValue<V> for V` — the identity blanket: any
///   value type can stand in as its own portable form (so existing
///   `field.eq(v)` call sites continue to compile against `DjogiField<M, V>`).
/// - `IntoPortableFieldValue<String> for &str` — wraps `&str` in `String`
///   for portable equality against String columns (issue #167).
/// - `impl<V> IntoPortableFieldValue<Option<V>> for V` and
///   `IntoPortableFieldValue<Option<String>> for &str` — wrap the inner value
///   in `Some(_)` for portable comparison against nullable columns. Sassi's
///   `Field<M, Option<V>>::eq(Some(v))` evaluates `None` rows as false,
///   matching SQL three-valued logic (issue #107).
/// - `impl<V> IntoPortableFieldValue<Tracked<V>> for V` and
///   `IntoPortableFieldValue<Tracked<String>> for &str` — wrap the inner
///   value in `Tracked::new(_)` (always clean). [`Tracked<T>`]'s
///   `PartialEq` / `PartialOrd` / `Ord` / `Hash` ignore the dirty flag, so
///   comparing a freshly-constructed `Tracked` against a loaded row's
///   `Tracked` reduces to comparing inner `T` values (issue #166).
pub trait IntoPortableFieldValue<FieldValue> {
    /// Convert `self` into the field's declared portable value type.
    ///
    /// The returned value flows into Sassi's `Field<M, FieldValue>::eq` /
    /// `.gt` / `.in_` / etc. as the right-hand-side operand. Implementors
    /// must produce a value whose [`PartialEq`] / [`PartialOrd`] / [`Hash`]
    /// behaviour matches the on-disk column ordering — otherwise Punnu and
    /// the database row will disagree at evaluation time.
    fn into_portable_field_value(self) -> FieldValue;
}

impl<V> IntoPortableFieldValue<V> for V {
    fn into_portable_field_value(self) -> V {
        self
    }
}

impl IntoPortableFieldValue<String> for &str {
    fn into_portable_field_value(self) -> String {
        self.to_owned()
    }
}

impl<V> IntoPortableFieldValue<Option<V>> for V {
    fn into_portable_field_value(self) -> Option<V> {
        Some(self)
    }
}

impl IntoPortableFieldValue<Option<String>> for &str {
    fn into_portable_field_value(self) -> Option<String> {
        Some(self.to_owned())
    }
}

impl<V> IntoPortableFieldValue<Tracked<V>> for V {
    fn into_portable_field_value(self) -> Tracked<V> {
        Tracked::new(self)
    }
}

impl IntoPortableFieldValue<Tracked<String>> for &str {
    fn into_portable_field_value(self) -> Tracked<String> {
        Tracked::new(self.to_owned())
    }
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
// Narrow integer widening (Phase 7-Zero-2 polish, GH issue #29; `u64`
// arm added under djogi#186 / Phase 8.5 v3 Cluster 2).
//
// Postgres has no native unsigned-integer types and no `i8`. Adopters
// who model fields as `u8` / `u16` / `u32` / `i8` (port numbers, small
// counts, signed-byte audio samples, etc.) need to compare against
// those values without manually upcasting. Each narrow type widens to
// the smallest signed Postgres type that fits its full range:
//
// - `i8`  → `I16`     (smallint)        — i8 fits in int2 directly.
// - `u8`  → `I16`     (smallint)        — u8 max 255 fits in int2's 32_767.
// - `u16` → `I32`     (integer)         — u16 max 65_535 exceeds i16's 32_767.
// - `u32` → `I64`     (bigint)          — u32 max ~4.3B exceeds i32's ~2.1B.
// - `u64` → `Decimal` (bare NUMERIC)     — u64 max ~18.4 quintillion exceeds
//                                         i64 max ~9.2 quintillion, so signed
//                                         widening loses the upper half. The
//                                         lossless route is `rust_decimal::Decimal`,
//                                         which Postgres `NUMERIC` accepts. The
//                                         column-side type-derived CHECK from
//                                         djogi#190 enforces both range bounds
//                                         and integrality (`col = trunc(col)`)
//                                         on the bare NUMERIC column.
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
impl IntoFilterValue for u64 {
    fn into_filter_value(self) -> FilterValue {
        // `Decimal::from(u64)` is infallible (u64::MAX < Decimal::MAX,
        // since Decimal is 96-bit mantissa). Round-trips exactly back
        // to u64 via the column-side decode path that djogi#190 will
        // wire when narrow / unsigned column support lands.
        FilterValue::Decimal(rust_decimal::Decimal::from(self))
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
impl IntoFilterValue for time::PrimitiveDateTime {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::Timestamp(self)
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
impl IntoFilterValue for crate::Interval {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::Interval(self)
    }
}
impl<T> IntoFilterValue for crate::Range<T>
where
    T: crate::range::RangeElement,
{
    fn into_filter_value(self) -> FilterValue {
        T::into_range_filter_value(self)
    }
}
// djogi#213 — network family. `IntoFilterValue` is feature-gated to
// match the FilterValue carrier variants. Equality on these types is
// structural in Rust AND in Postgres (INET / CIDR / MACADDR `=` compare
// bytes), so they ride the `DjogiPortableEq` path without an SQL-only
// escape hatch the way `Interval` requires.
#[cfg(feature = "network")]
impl IntoFilterValue for std::net::IpAddr {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::Inet(self)
    }
}
#[cfg(feature = "network")]
impl IntoFilterValue for crate::CidrAddr {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::Cidr(self)
    }
}
#[cfg(feature = "network")]
impl IntoFilterValue for crate::MacAddr {
    fn into_filter_value(self) -> FilterValue {
        FilterValue::Macaddr(self)
    }
}
impl<V> IntoFilterValue for Vec<V>
where
    V: IntoArrayFilterValue,
{
    fn into_filter_value(self) -> FilterValue {
        V::into_array_filter_value(self)
    }
}

// ── Generic lookup methods (any V: IntoFilterValue) ───────────────────────

impl<M: Model, V> FieldRef<M, V> {
    /// `column = value` — SQL equality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn eq<P>(self, value: P) -> Condition
    where
        P: IntoFieldFilterValue<V>,
    {
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::Eq,
            value.into_field_filter_value(),
        ))
    }

    /// `column <> value` — SQL inequality.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn neq<P>(self, value: P) -> Condition
    where
        P: IntoFieldFilterValue<V>,
    {
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::Neq,
            value.into_field_filter_value(),
        ))
    }

    /// `column > value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn gt<P>(self, value: P) -> Condition
    where
        P: IntoFieldFilterValue<V>,
    {
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::Gt,
            value.into_field_filter_value(),
        ))
    }

    /// `column >= value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn gte<P>(self, value: P) -> Condition
    where
        P: IntoFieldFilterValue<V>,
    {
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::Gte,
            value.into_field_filter_value(),
        ))
    }

    /// `column < value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn lt<P>(self, value: P) -> Condition
    where
        P: IntoFieldFilterValue<V>,
    {
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::Lt,
            value.into_field_filter_value(),
        ))
    }

    /// `column <= value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn lte<P>(self, value: P) -> Condition
    where
        P: IntoFieldFilterValue<V>,
    {
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::Lte,
            value.into_field_filter_value(),
        ))
    }

    /// `column BETWEEN a AND b` (inclusive on both ends per SQL spec).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn between<P>(self, a: P, b: P) -> Condition
    where
        P: IntoFieldFilterValue<V>,
    {
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::Between,
            FilterValue::Pair(
                Box::new(a.into_field_filter_value()),
                Box::new(b.into_field_filter_value()),
            ),
        ))
    }

    /// Case-insensitive equality — `LOWER(column) = LOWER(value)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn iexact<P>(self, value: P) -> Condition
    where
        P: IntoFieldFilterValue<V>,
    {
        Condition::Leaf(Leaf::new(
            self.column,
            LookupOp::IExact,
            value.into_field_filter_value(),
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
impl<M: Model, V> FieldRef<M, V> {
    /// `column IN (v1, v2, …)`. An empty iterator is allowed and renders as
    /// SQL `FALSE` at emission time (Task 6).
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn in_list<I, P>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = P>,
        P: IntoFieldFilterValue<V>,
    {
        let list = FilterValue::List(
            values
                .into_iter()
                .map(P::into_field_filter_value)
                .collect::<Vec<_>>(),
        );
        Condition::Leaf(Leaf::new(self.column, LookupOp::In, list))
    }

    /// `column NOT IN (v1, v2, …)`. An empty iterator is allowed and renders
    /// as SQL `TRUE` at emission time.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_in_list<I, P>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = P>,
        P: IntoFieldFilterValue<V>,
    {
        let list = FilterValue::List(
            values
                .into_iter()
                .map(P::into_field_filter_value)
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

// ── pg_trgm — trigram similarity lookups (gated on `trgm` feature) ──────────
//
// Implementation on `FieldRef<M, String>` (the SQL-only handle). The public
// surface is forwarded from:
//   - `ExplicitPgPredicateField<M, String>::trgm_similar_to` — predicate
//     compiling to the `%` operator (`WHERE <col> % $n`). Reached via
//     `f.col().explicit_pg_predicate().trgm_similar_to(pattern)`. Index-
//     accelerated by `gin_trgm_ops` / `gist_trgm_ops`. Threshold is the
//     session GUC `pg_trgm.similarity_threshold`.
//   - `DjogiField<M, String>::trgm_similarity` — score expression
//     (`similarity(col, $n)`). Reached via `f.col().trgm_similarity(pattern)`
//     in `filter_expr` closures for per-query numeric thresholds. NOT
//     accelerated by the trgm opclasses (function-form predicate, not
//     operator).
//
// The pattern always rides through `push_bind` as a positional parameter —
// no user text is interpolated into SQL.
#[cfg(feature = "trgm")]
impl<M: Model> FieldRef<M, String> {
    /// Trigram similarity predicate — `<col> % $pattern`.
    ///
    /// **Adopters:** reach this via
    /// `f.col().explicit_pg_predicate().trgm_similar_to(pattern)` (the same
    /// path as `regex`/`iregex`). This `FieldRef` method is the
    /// implementation target; `ExplicitPgPredicateField` forwards to it.
    ///
    /// Returns a `Condition` that filters rows where the column value is
    /// trigram-similar to `pattern` under Postgres's `%` operator. The
    /// `%` operator is the indexable strategy member of the
    /// `gin_trgm_ops` and `gist_trgm_ops` opclasses, so a GIN or GiST
    /// index built with one of those opclasses accelerates this
    /// predicate.
    ///
    /// # Threshold
    ///
    /// The threshold for `%` is the session GUC
    /// `pg_trgm.similarity_threshold` (Postgres default `0.3`). Override
    /// per-session with `SET pg_trgm.similarity_threshold = 0.4;` or
    /// per-transaction with `SET LOCAL ...` inside a `BEGIN`/`COMMIT`
    /// block.
    ///
    /// For a per-query numeric threshold without touching the GUC, use
    /// [`Self::trgm_similarity`] inside `filter_expr` with the
    /// `Expr<T>` comparison API. That form is NOT accelerated by the
    /// trgm opclasses.
    ///
    /// # Extension requirement
    ///
    /// Requires `pg_trgm` installed in the target Postgres database:
    ///
    /// ```sql
    /// CREATE EXTENSION IF NOT EXISTS pg_trgm;
    /// ```
    ///
    /// Djogi's migration runner installs `pg_trgm` automatically through
    /// the Phase 0 bootstrap migration when any index in the descriptor
    /// inventory declares `extension_dependency: Some("pg_trgm")`. See
    /// `docs/guide/trgm.md` for the per-app vs Phase 0 split.
    ///
    /// # Index acceleration
    ///
    /// Declare a GIN index with `gin_trgm_ops` opclass for high read
    /// throughput; GiST with `gist_trgm_ops` for queries that also use
    /// the `<->` distance operator (not yet exposed at the typed surface).
    /// Without a trgm-opclass index, the `%` operator falls back to a
    /// sequential scan with per-row similarity computation.
    ///
    /// # SQL shape
    ///
    /// Emits: `<col> % $1`
    ///
    /// The `pattern` value is a positional bind parameter — never
    /// interpolated into SQL.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn trgm_similar_to(self, pattern: impl Into<String>) -> Condition {
        Condition::Expr(crate::expr::Expr::from_node(
            crate::expr::node::ExprNode::TrgmSimilarTo {
                column: self.column,
                pattern: pattern.into(),
            },
        ))
    }

    /// Trigram similarity score expression —
    /// `similarity(col, $pattern)`.
    ///
    /// **Adopters:** reach this via `f.col().trgm_similarity(pattern)` directly
    /// on the `DjogiField` returned by generated `{Model}Fields` accessors.
    /// This `FieldRef` method is the implementation target that both
    /// `DjogiField::trgm_similarity` and direct `FieldRef` callsites forward to.
    ///
    /// Returns an `Expr<f64>` that Postgres evaluates per row as the
    /// `similarity()` value (`[0.0, 1.0]`) between the column and the
    /// supplied pattern. Compose with the `Expr<T>` comparison API
    /// inside `filter_expr` to apply a per-query numeric threshold:
    ///
    /// ```ignore
    /// qs.filter_expr(|f| {
    ///     f.bio().trgm_similarity("query").gte(Expr::literal(0.3_f64))
    /// })
    /// ```
    ///
    /// # Index acceleration
    ///
    /// The function-form predicate emitted by `trgm_similarity(...).gte(...)`
    /// is **not** accelerated by `gin_trgm_ops` / `gist_trgm_ops` — those
    /// opclasses target the operator family (`%`, `<%`, `<<%`, `<->`,
    /// `<<->`, `<<<->`, `=`), not arbitrary `similarity(...)` comparisons.
    /// For index-accelerated trgm scans, use [`Self::trgm_similar_to`].
    ///
    /// # Future work
    ///
    /// Using `Expr<f64>` as an `order_by` target or as an `annotate`
    /// payload requires generic `Expr<T>` integration on `OrderExpr`
    /// and `AnnotationSlot`, which is not yet implemented. The same
    /// gap affects `TsRank` / `TsRankCd` in the FTS feature and any
    /// future score-producing expression. See `docs/guide/trgm.md`
    /// for the documented limitations and the tracked follow-up.
    ///
    /// # Extension requirement
    ///
    /// Requires `pg_trgm` installed in the target Postgres database.
    ///
    /// # SQL shape
    ///
    /// Emits: `similarity(<col>, $1)`
    ///
    /// The `pattern` value is a positional bind parameter — never
    /// interpolated into SQL.
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn trgm_similarity(self, pattern: impl Into<String>) -> crate::expr::Expr<f64> {
        crate::expr::Expr::from_node(crate::expr::node::ExprNode::TrgmSimilarityScore {
            column: self.column,
            pattern: pattern.into(),
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
    impl Sealed for i16 {}
    impl Sealed for i32 {}
    impl Sealed for i64 {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}
    impl Sealed for bool {}
    impl Sealed for time::OffsetDateTime {}
    impl Sealed for time::Date {}
    impl Sealed for uuid::Uuid {}
    impl Sealed for rust_decimal::Decimal {}
    impl Sealed for crate::types::HeerId {}
    impl Sealed for crate::types::RanjId {}
    impl Sealed for crate::types::HeerIdDesc {}
    impl Sealed for crate::types::RanjIdDesc {}
}

/// Converts a `Vec<V>` element type into the matching [`FilterValue::Array*`]
/// variant for use in array operator conditions.
///
/// Sealed so that only the Djogi-blessed array element types can be used with
/// the array operator methods on `FieldRef<M, Vec<V>>`. Downstream code cannot
/// implement this trait.
///
/// # Supported element types
///
/// | Rust type | Postgres column type |
/// |---|---|
/// | `String` | `TEXT[]` |
/// | `i16` | `SMALLINT[]` |
/// | `i32` | `INTEGER[]` |
/// | `i64` | `BIGINT[]` |
/// | `f32` | `REAL[]` |
/// | `f64` | `DOUBLE PRECISION[]` |
/// | `bool` | `BOOLEAN[]` |
/// | `time::OffsetDateTime` | `TIMESTAMPTZ[]` |
/// | `time::Date` | `DATE[]` |
/// | `uuid::Uuid` | `UUID[]` |
/// | `rust_decimal::Decimal` | `NUMERIC[]` |
/// | [`HeerId`](crate::types::HeerId) | `BIGINT[]` |
/// | [`RanjId`](crate::types::RanjId) | `UUID[]` |
/// | [`HeerIdDesc`](crate::types::HeerIdDesc) / [`HeerIdRecencyBiased`](crate::types::HeerIdRecencyBiased) | `BIGINT[]` |
/// | [`RanjIdDesc`](crate::types::RanjIdDesc) / [`RanjIdRecencyBiased`](crate::types::RanjIdRecencyBiased) | `UUID[]` |
///
/// For adopter-defined newtype or enum element types, see the
/// [array guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/arrays.md)
/// for the `DjogiSqlType` extension path.
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
impl IntoArrayFilterValue for i16 {
    fn into_array_filter_value(values: Vec<Self>) -> FilterValue {
        FilterValue::ArrayI16(values)
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
impl IntoArrayFilterValue for f32 {
    fn into_array_filter_value(values: Vec<Self>) -> FilterValue {
        FilterValue::ArrayF32(values)
    }
}
impl IntoArrayFilterValue for f64 {
    fn into_array_filter_value(values: Vec<Self>) -> FilterValue {
        FilterValue::ArrayF64(values)
    }
}
impl IntoArrayFilterValue for bool {
    fn into_array_filter_value(values: Vec<Self>) -> FilterValue {
        FilterValue::ArrayBool(values)
    }
}
impl IntoArrayFilterValue for time::OffsetDateTime {
    fn into_array_filter_value(values: Vec<Self>) -> FilterValue {
        FilterValue::ArrayDateTime(values)
    }
}
impl IntoArrayFilterValue for time::Date {
    fn into_array_filter_value(values: Vec<Self>) -> FilterValue {
        FilterValue::ArrayDate(values)
    }
}
impl IntoArrayFilterValue for uuid::Uuid {
    fn into_array_filter_value(values: Vec<Self>) -> FilterValue {
        FilterValue::ArrayUuid(values)
    }
}
impl IntoArrayFilterValue for rust_decimal::Decimal {
    fn into_array_filter_value(values: Vec<Self>) -> FilterValue {
        FilterValue::ArrayDecimal(values)
    }
}
impl IntoArrayFilterValue for crate::types::HeerId {
    fn into_array_filter_value(values: Vec<Self>) -> FilterValue {
        FilterValue::ArrayHeerId(values)
    }
}
impl IntoArrayFilterValue for crate::types::RanjId {
    fn into_array_filter_value(values: Vec<Self>) -> FilterValue {
        FilterValue::ArrayRanjId(values)
    }
}
impl IntoArrayFilterValue for crate::types::HeerIdDesc {
    fn into_array_filter_value(values: Vec<Self>) -> FilterValue {
        FilterValue::ArrayHeerIdDesc(values)
    }
}
impl IntoArrayFilterValue for crate::types::RanjIdDesc {
    fn into_array_filter_value(values: Vec<Self>) -> FilterValue {
        FilterValue::ArrayRanjIdDesc(values)
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

impl<M: Model, T> FieldRef<M, crate::Range<T>>
where
    T: crate::range::RangeElement + IntoFilterValue,
{
    fn range_predicate(self, op: crate::range::RangePredicateOp, value: FilterValue) -> Condition {
        Condition::RangePredicate(crate::range::RangePredicateLeaf::new(
            self.column,
            op,
            value,
        ))
    }

    /// `column @> value` — range contains an element.
    ///
    /// PostgreSQL-specific; root model fields expose this through
    /// [`DjogiField::explicit_pg_predicate`].
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn contains(self, value: T) -> Condition {
        Condition::RangePredicate(
            crate::range::RangePredicateLeaf::new(
                self.column,
                crate::range::RangePredicateOp::Contains,
                value.into_filter_value(),
            )
            .with_rhs_element_cast(T::sql_element_cast()),
        )
    }

    /// `column OP rhs` — shared range/range operator payload path.
    fn range_rhs_predicate(
        self,
        op: crate::range::RangePredicateOp,
        range: crate::Range<T>,
    ) -> Condition {
        self.range_predicate(op, T::into_range_filter_value(range))
    }

    /// `column @> range` — range contains another range.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn contains_range(self, range: crate::Range<T>) -> Condition {
        self.range_rhs_predicate(crate::range::RangePredicateOp::Contains, range)
    }

    /// `column <@ range` — range is contained by another range.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn contained_by(self, range: crate::Range<T>) -> Condition {
        self.range_rhs_predicate(crate::range::RangePredicateOp::ContainedBy, range)
    }

    /// `column && range` — ranges overlap.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn overlaps(self, range: crate::Range<T>) -> Condition {
        self.range_rhs_predicate(crate::range::RangePredicateOp::Overlaps, range)
    }

    /// `column << range` — range is strictly left of another range.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn strictly_left_of(self, range: crate::Range<T>) -> Condition {
        self.range_rhs_predicate(crate::range::RangePredicateOp::StrictlyLeftOf, range)
    }

    /// `column >> range` — range is strictly right of another range.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn strictly_right_of(self, range: crate::Range<T>) -> Condition {
        self.range_rhs_predicate(crate::range::RangePredicateOp::StrictlyRightOf, range)
    }

    /// `column &< range` — range does not extend right of another range.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_extends_right_of(self, range: crate::Range<T>) -> Condition {
        self.range_rhs_predicate(crate::range::RangePredicateOp::NotExtendsRightOf, range)
    }

    /// `column &> range` — range does not extend left of another range.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not_extends_left_of(self, range: crate::Range<T>) -> Condition {
        self.range_rhs_predicate(crate::range::RangePredicateOp::NotExtendsLeftOf, range)
    }

    /// `column -|- range` — ranges are adjacent.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn adjacent_to(self, range: crate::Range<T>) -> Condition {
        self.range_rhs_predicate(crate::range::RangePredicateOp::AdjacentTo, range)
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
// decode.

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
    /// - [`crate::expr::node::AggOp::SpatialConvexHull`] — IR variant
    ///   that the typed surface stores inside the `AggregateExpr` envelope.
    /// - [`crate::query::QuerySet::group_by`] — produces the
    ///   `GroupedQuerySet` that consumes this aggregate via `.annotate(...)`.
    ///
    /// # Modifier composition
    ///
    /// Routes through the `Aggregate` envelope so all modifiers
    /// (`.distinct()` / `.filter()` / `.over()` / `.order_by()`)
    /// compose uniformly with the rest of the spatial aggregate
    /// family. The wrapped emission shape
    /// `ST_ConvexHull(ST_Collect(...) FILTER (...) OVER (...))::geography`
    /// places the modifiers on the inner `ST_Collect` aggregate
    /// (which is the actual aggregating step), inside the
    /// `ST_ConvexHull` scalar wrapper, before the outer cast.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn convex_hull(self) -> crate::expr::AggregateExpr<crate::geo::Polygon> {
        // Cluster E round-5 BLOCK-2 closure: migrated from
        // ExprNode::Spatial(SpatialExpr::ConvexHull{..}) to
        // AggOp::SpatialConvexHull so AggregateExpr modifiers
        // (.distinct/.filter/.over/.order_by) compose uniformly.
        // The old IR shape silently dropped these modifiers because
        // the modifier impls only mutate ExprNode::Aggregate.
        crate::expr::AggregateExpr::unary_agg(
            crate::expr::node::AggOp::SpatialConvexHull,
            self.column(),
            None,
        )
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
    use crate::__private::pg::SqlAccumulator;
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

    #[derive(Debug)]
    struct FakeRow {
        id: i64,
        age: i64,
        title: String,
        maybe_age: Option<i64>,
        duration: crate::Interval,
        maybe_duration: Option<crate::Interval>,
        span: crate::Range<i32>,
    }

    impl crate::model::__sealed::Sealed for FakeRow {}
    #[allow(clippy::manual_async_fn)]
    impl crate::model::Model for FakeRow {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "fake_rows"
        }
        fn pk_value(&self) -> &i64 {
            &self.id
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
    fn djogi_field_eq_returns_portable_sassi_leaf() {
        let f =
            djogi_field_macro_support::__make_djogi_field::<FakeRow, i64>("age", |row| &row.age);
        let predicate = f.eq(42).into_inner();

        match predicate {
            sassi::BasicPredicate::Field(field) => {
                assert_eq!(field.field_name(), "age");
                assert_eq!(field.op(), sassi::LookupOp::Eq);
                assert_eq!(field.value_as::<i64>(), Some(&42));
            }
            other => panic!("expected Field predicate, got {other:?}"),
        }
    }

    #[test]
    fn djogi_field_optional_eq_and_some_eq_use_planned_payloads() {
        let f = djogi_field_macro_support::__make_djogi_field::<FakeRow, Option<i64>>(
            "maybe_age",
            |row| &row.maybe_age,
        );

        match f.eq(None).into_inner() {
            sassi::BasicPredicate::Field(field) => {
                assert_eq!(field.field_name(), "maybe_age");
                assert_eq!(field.op(), sassi::LookupOp::Eq);
                assert_eq!(field.value_as::<Option<i64>>(), Some(&None));
            }
            other => panic!("expected optional Eq field predicate, got {other:?}"),
        }

        match f.some().eq(7).into_inner() {
            sassi::BasicPredicate::Field(field) => {
                assert_eq!(field.field_name(), "maybe_age");
                assert_eq!(field.op(), sassi::LookupOp::Eq);
                assert_eq!(field.value_as::<i64>(), Some(&7));
            }
            other => panic!("expected present Eq field predicate, got {other:?}"),
        }
    }

    #[test]
    fn djogi_field_interval_equality_and_membership_are_sql_only_conditions() {
        let f = djogi_field_macro_support::__make_djogi_field::<FakeRow, crate::Interval>(
            "duration",
            |row| &row.duration,
        );
        let one_month = crate::Interval::months_only(1);

        let eq: Condition = f.eq(one_month);
        let neq: Condition = f.neq(one_month);
        let in_list: Condition = f.in_([one_month]);
        let not_in_list: Condition = f.not_in([one_month]);

        if let Condition::Leaf(leaf) = eq {
            assert_eq!(leaf.column, "duration");
            assert_eq!(leaf.op, LookupOp::Eq);
            assert!(matches!(leaf.value, FilterValue::Interval(v) if v == one_month));
        } else {
            panic!("expected Interval Eq to produce a SQL-only Condition leaf");
        }

        if let Condition::Leaf(leaf) = neq {
            assert_eq!(leaf.op, LookupOp::Neq);
        } else {
            panic!("expected Interval Neq to produce a SQL-only Condition leaf");
        }

        if let Condition::Leaf(leaf) = in_list {
            assert_eq!(leaf.op, LookupOp::In);
        } else {
            panic!("expected Interval IN to produce a SQL-only Condition leaf");
        }

        if let Condition::Leaf(leaf) = not_in_list {
            assert_eq!(leaf.op, LookupOp::NotIn);
        } else {
            panic!("expected Interval NOT IN to produce a SQL-only Condition leaf");
        }
    }

    #[test]
    fn range_predicates_build_typed_range_condition_payloads() {
        let field = FieldRef::<Fake, crate::Range<i32>>::new("span");
        let probe = crate::Range::inclusive_exclusive(2_i32, 4_i32);

        let contains_element = field.contains(3_i32);
        if let Condition::RangePredicate(leaf) = contains_element {
            assert_eq!(leaf.column(), "span");
            assert_eq!(leaf.op(), crate::range::RangePredicateOp::Contains);
            assert!(matches!(leaf.value(), FilterValue::I32(3)));
            assert_eq!(leaf.rhs_element_cast(), Some("int4"));
        } else {
            panic!("expected contains(element) to produce a range predicate condition");
        }

        let overlaps = field.overlaps(probe);
        if let Condition::RangePredicate(leaf) = overlaps {
            assert_eq!(leaf.column(), "span");
            assert_eq!(leaf.op(), crate::range::RangePredicateOp::Overlaps);
            assert!(matches!(leaf.value(), FilterValue::RangeI32(range) if range == &probe));
            assert_eq!(leaf.rhs_element_cast(), None);
        } else {
            panic!("expected overlaps(range) to produce a range predicate condition");
        }
    }

    #[test]
    fn explicit_pg_range_predicates_forward_from_root_field() {
        let f = djogi_field_macro_support::__make_djogi_field::<FakeRow, crate::Range<i32>>(
            "span",
            |row| &row.span,
        );
        let probe = crate::Range::inclusive_exclusive(2_i32, 4_i32);

        let adjacent = f.explicit_pg_predicate().adjacent_to(probe);
        if let Condition::RangePredicate(leaf) = adjacent {
            assert_eq!(leaf.column(), "span");
            assert_eq!(leaf.op(), crate::range::RangePredicateOp::AdjacentTo);
            assert!(matches!(leaf.value(), FilterValue::RangeI32(range) if range == &probe));
        } else {
            panic!("expected explicit range predicate to produce a range predicate condition");
        }
    }

    #[test]
    fn djogi_field_nullable_interval_eq_and_some_eq_are_sql_only_conditions() {
        let f = djogi_field_macro_support::__make_djogi_field::<FakeRow, Option<crate::Interval>>(
            "maybe_duration",
            |row| &row.maybe_duration,
        );
        let thirty_days = crate::Interval::days_only(30);

        let nullable_eq: Condition = f.eq(thirty_days);
        let present_eq: Condition = f.some().eq(thirty_days);

        if let Condition::Leaf(leaf) = nullable_eq {
            assert_eq!(leaf.column, "maybe_duration");
            assert_eq!(leaf.op, LookupOp::Eq);
            assert!(matches!(leaf.value, FilterValue::Interval(v) if v == thirty_days));
        } else {
            panic!("expected nullable Interval Eq to produce a SQL-only Condition leaf");
        }

        if let Condition::Leaf(leaf) = present_eq {
            assert_eq!(leaf.column, "maybe_duration");
            assert_eq!(leaf.op, LookupOp::Eq);
            assert!(matches!(leaf.value, FilterValue::Interval(v) if v == thirty_days));
        } else {
            panic!("expected nullable Interval some().eq to produce a SQL-only Condition leaf");
        }

        let direct_empty_not_in: Condition = f.not_in(Vec::<crate::Interval>::new());
        let mut acc = SqlAccumulator::new("");
        crate::query::sql::emit_condition(&mut acc, &direct_empty_not_in, None).unwrap();
        assert_eq!(
            acc.sql(),
            "TRUE",
            "direct nullable Interval not_in([]) should keep the shared empty-list convention"
        );

        let present_empty_not_in: Condition = f.some().not_in(Vec::<crate::Interval>::new());
        let mut acc = SqlAccumulator::new("");
        crate::query::sql::emit_condition(&mut acc, &present_empty_not_in, None).unwrap();
        assert_eq!(
            acc.sql(),
            "maybe_duration IS NOT NULL",
            "present Interval not_in([]) must preserve the IS NOT NULL guard"
        );
    }

    #[test]
    fn djogi_field_null_checks_stay_portable() {
        let f = djogi_field_macro_support::__make_djogi_field::<FakeRow, Option<i64>>(
            "maybe_age",
            |row| &row.maybe_age,
        );

        match f.is_null().into_inner() {
            sassi::BasicPredicate::Field(field) => {
                assert_eq!(field.field_name(), "maybe_age");
                assert_eq!(field.op(), sassi::LookupOp::IsNull);
                assert_eq!(field.value_as::<()>(), Some(&()));
            }
            other => panic!("expected IsNull field predicate, got {other:?}"),
        }
    }

    /// FIX_BEFORE_BETA-4: the new
    /// `impl<M, U> DjogiField<M, Option<U>> where U: DjogiPortableOrd`
    /// block routes `gt` / `gte` / `lt` / `lte` / `between` through the
    /// present-field surface (`self.some().gt(value)` and friends).
    /// The resulting `PortablePredicate` must carry the inner-`U` payload
    /// (matching `DjogiField<M, Option<U>>::some().gt(value)`) and the
    /// underlying Sassi op — anything else means the routing is wrong
    /// and the runtime emit will fail with `ValueTypeMismatch` because
    /// `option_arms`'s ordering arms downcast to `U`, not `Option<U>`.
    #[test]
    fn djogi_field_optional_ordering_routes_through_present_field_payloads() {
        let f = djogi_field_macro_support::__make_djogi_field::<FakeRow, Option<i64>>(
            "maybe_age",
            |row| &row.maybe_age,
        );

        for (predicate, expected_op) in [
            (f.gt(7_i64).into_inner(), sassi::LookupOp::Gt),
            (f.gte(7_i64).into_inner(), sassi::LookupOp::Gte),
            (f.lt(7_i64).into_inner(), sassi::LookupOp::Lt),
            (f.lte(7_i64).into_inner(), sassi::LookupOp::Lte),
        ] {
            match predicate {
                sassi::BasicPredicate::Field(field) => {
                    assert_eq!(field.field_name(), "maybe_age");
                    assert_eq!(field.op(), expected_op);
                    // The macro-emitted `option_arms` ordering branch
                    // downcasts the type-erased payload as the inner
                    // `U` (here `i64`) — never `Option<i64>` — because
                    // `Option<U>` ordering would disagree with SQL
                    // three-valued NULL semantics.
                    assert_eq!(
                        field.value_as::<i64>(),
                        Some(&7),
                        "ordering arm payload must be inner U, not Option<U>",
                    );
                    assert!(
                        field.value_as::<Option<i64>>().is_none(),
                        "Option<U> downcast must NOT match — that would route to a NULL-aware arm by mistake",
                    );
                }
                other => panic!("expected Field predicate, got {other:?}"),
            }
        }
    }

    #[test]
    fn djogi_field_optional_between_routes_through_present_field_pair() {
        let f = djogi_field_macro_support::__make_djogi_field::<FakeRow, Option<i64>>(
            "maybe_age",
            |row| &row.maybe_age,
        );

        match f.between(10_i64, 20_i64).into_inner() {
            sassi::BasicPredicate::Field(field) => {
                assert_eq!(field.field_name(), "maybe_age");
                assert_eq!(field.op(), sassi::LookupOp::Between);
                // `option_arms`'s Between branch under `supports_ordering`
                // calls `emit_pair::<M, U>` which downcasts as `(U, U)` —
                // confirm the payload matches the contract.
                assert_eq!(field.value_as::<(i64, i64)>(), Some(&(10_i64, 20_i64)));
                assert!(field.value_as::<(Option<i64>, Option<i64>)>().is_none());
            }
            other => panic!("expected Between field predicate, got {other:?}"),
        }
    }

    /// Issue #167 coverage extension — `Option<String>` field accepts
    /// `&str` lookup arguments. Mirrors the non-Option `String` ergonomics
    /// for nullable text columns.
    #[test]
    fn djogi_field_optional_string_accepts_borrowed_str_for_eq() {
        // FakeRow doesn't carry an Option<String>, but `__make_djogi_field`
        // accepts any extractor function; reuse `title` and reinterpret the
        // value for the test. The call shape is what matters.
        let f = djogi_field_macro_support::__make_djogi_field::<FakeRow, Option<String>>(
            "tagline",
            |_row| {
                // Stable per-call reference into a static `None`. The
                // closure is never executed in this unit test (no
                // matching against an actual row); its existence
                // satisfies the `extract: fn(&M) -> &V` signature only.
                static NONE: Option<String> = None;
                &NONE
            },
        );

        match f.eq("hello").into_inner() {
            sassi::BasicPredicate::Field(field) => {
                assert_eq!(field.field_name(), "tagline");
                assert_eq!(field.op(), sassi::LookupOp::Eq);
                // `IntoPortableFieldValue<Option<String>> for &str` (issue
                // #167) wraps `"hello"` as `Some("hello".to_owned())` for
                // the Sassi payload, so the direct Option<String> downcast
                // matches.
                assert_eq!(
                    field.value_as::<Option<String>>(),
                    Some(&Some("hello".to_owned())),
                );
            }
            other => panic!("expected Eq field predicate, got {other:?}"),
        }
    }

    #[test]
    fn djogi_field_string_contains_routes_to_portable_case_contract() {
        let f = djogi_field_macro_support::__make_djogi_field::<FakeRow, String>("title", |row| {
            &row.title
        });

        match f.contains("Rust").into_inner() {
            sassi::BasicPredicate::Field(field) => {
                assert_eq!(field.field_name(), "title");
                assert_eq!(field.op(), sassi::LookupOp::IContains);
                assert_eq!(field.value_as::<String>(), Some(&"Rust".to_string()));
            }
            other => panic!("expected IContains field predicate, got {other:?}"),
        }

        match f.contains_case_sensitive("Rust").into_inner() {
            sassi::BasicPredicate::Field(field) => {
                assert_eq!(field.field_name(), "title");
                assert_eq!(field.op(), sassi::LookupOp::Contains);
                assert_eq!(field.value_as::<String>(), Some(&"Rust".to_string()));
            }
            other => panic!("expected Contains field predicate, got {other:?}"),
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
        let _ = g.eq("a");
        let _ = h.neq("b");
    }

    #[test]
    fn field_ref_option_scalar_accepts_inner_value_lookup() {
        let f: FieldRef<Fake, Option<i16>> = FieldRef::new("estimated_birth_year");
        let c = f.lte(1990_i16);
        if let Condition::Leaf(leaf) = c {
            assert_eq!(leaf.column, "estimated_birth_year");
            assert_eq!(leaf.op, LookupOp::Lte);
            assert!(matches!(leaf.value, FilterValue::I16(1990)));
        } else {
            panic!("expected Leaf");
        }
    }

    #[test]
    fn field_ref_tracked_string_accepts_inner_str_lookup() {
        let f: FieldRef<Fake, Tracked<String>> = FieldRef::new("label");
        let c = f.eq("alpha");
        if let Condition::Leaf(leaf) = c {
            assert_eq!(leaf.column, "label");
            assert_eq!(leaf.op, LookupOp::Eq);
            assert!(matches!(leaf.value, FilterValue::String(ref s) if s == "alpha"));
        } else {
            panic!("expected Leaf");
        }
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

    #[test]
    fn djogi_field_json_object_agg_accepts_djogi_field_argument() {
        let f_id: DjogiField<FakeRow, i64> =
            djogi_field_macro_support::__make_djogi_field::<FakeRow, i64>("id", |row| &row.id);
        let f_name: DjogiField<FakeRow, String> = djogi_field_macro_support::__make_djogi_field::<
            FakeRow,
            String,
        >("title", |row| &row.title);
        let _: crate::expr::AggregateExpr<serde_json::Value> = f_id.json_object_agg(f_name);
    }

    #[test]
    fn djogi_field_jsonb_object_agg_accepts_djogi_field_argument() {
        let f_id: DjogiField<FakeRow, i64> =
            djogi_field_macro_support::__make_djogi_field::<FakeRow, i64>("id", |row| &row.id);
        let f_name: DjogiField<FakeRow, Option<i64>> =
            djogi_field_macro_support::__make_djogi_field::<FakeRow, Option<i64>>(
                "maybe_age",
                |row| &row.maybe_age,
            );
        let _: crate::expr::AggregateExpr<serde_json::Value> = f_id.jsonb_object_agg(f_name);
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
    /// `AggOp::SpatialConvexHull` aggregate IR variant. The typed
    /// return is a compile-time assertion; the runtime check pins the
    /// stored field column.
    ///
    /// Cluster E round-5 BLOCK-2 closure: ConvexHull migrated from
    /// `ExprNode::Spatial(SpatialExpr::ConvexHull{..})` to a proper
    /// `AggOp::SpatialConvexHull` so AggregateExpr modifiers
    /// (`.distinct()` / `.filter()` / `.over()` / `.order_by()`)
    /// compose uniformly with the rest of the aggregate family.
    #[cfg(feature = "spatial")]
    #[test]
    fn convex_hull_on_geopoint_field_produces_aggregate_polygon() {
        use crate::expr::AggregateExpr;
        use crate::expr::node::AggOp;
        use crate::geo::Polygon as PolygonTy;

        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        // Compile-time check on the return type.
        let agg: AggregateExpr<PolygonTy> = field.convex_hull();
        // Runtime check — the IR node is the AggOp::SpatialConvexHull variant.
        if let ExprNode::Aggregate { op, arg, .. } = agg.node {
            assert!(matches!(op, AggOp::SpatialConvexHull));
            if let ExprNode::Field { column } = *arg {
                assert_eq!(column, "location");
                return;
            }
            panic!("expected Aggregate.arg to wrap the column");
        }
        panic!("expected ExprNode::Aggregate(SpatialConvexHull)");
    }

    /// `convex_hull` is generic over `GeographyValue` — verify it dispatches
    /// from a `Polygon` field as well as a `GeoPoint` field. The mating-pairs
    /// demo uses it on point columns; spec keeps the generic surface so
    /// callers with polygonal range columns can fold those into a hull too.
    #[cfg(feature = "spatial")]
    #[test]
    fn convex_hull_dispatches_from_polygon_field_too() {
        use crate::expr::AggregateExpr;
        use crate::expr::node::AggOp;
        use crate::geo::Polygon as PolygonTy;

        let field: FieldRef<Fake, PolygonTy> = FieldRef::new("territory");
        let agg: AggregateExpr<PolygonTy> = field.convex_hull();
        if let ExprNode::Aggregate { op, arg, .. } = agg.node {
            assert!(matches!(op, AggOp::SpatialConvexHull));
            if let ExprNode::Field { column } = *arg {
                assert_eq!(column, "territory");
                return;
            }
            panic!("expected Aggregate.arg to wrap the column");
        }
        panic!("expected ConvexHull AggOp on Polygon field");
    }

    /// Bare emission shape for `convex_hull` — the canonical PostGIS
    /// pattern `ST_ConvexHull(ST_Collect(<col>::geometry))::geography`.
    /// Inner `::geometry` cast for ST_Collect's geometry-only signature;
    /// outer `::geography` cast keeps the typed `Polygon` decode sound.
    /// Replaces the round-3 SpatialExpr-routed test that was removed
    /// when ConvexHull migrated to `AggOp::SpatialConvexHull`.
    #[cfg(feature = "spatial")]
    #[test]
    fn convex_hull_emits_st_convexhull_st_collect_with_geography_cast() {
        use crate::pg::accumulator::SqlAccumulator;
        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let agg = field.convex_hull();
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
        assert_eq!(
            acc.sql(),
            "ST_ConvexHull(ST_Collect(location::geometry))::geography"
        );
    }

    /// `.distinct()` on convex_hull lands inside ST_Collect (the actual
    /// aggregate). Cluster E round-5 BLOCK-2 closure regression: before
    /// the AggOp migration this modifier silently no-op'd because
    /// AggregateExpr modifiers only mutate ExprNode::Aggregate.
    #[cfg(feature = "spatial")]
    #[test]
    fn convex_hull_distinct_lands_inside_st_collect() {
        use crate::pg::accumulator::SqlAccumulator;
        let field: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let agg = field.convex_hull().distinct();
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
        assert_eq!(
            acc.sql(),
            "ST_ConvexHull(ST_Collect(DISTINCT location::geometry))::geography"
        );
    }

    /// `.filter(...)` on convex_hull places FILTER inside the wrapper,
    /// attached to ST_Collect. Cluster E round-5 BLOCK-2 closure
    /// regression — pre-migration this modifier silently no-op'd.
    #[cfg(feature = "spatial")]
    #[test]
    fn convex_hull_filter_attaches_to_inner_st_collect() {
        use crate::expr::Expr;
        use crate::pg::accumulator::SqlAccumulator;
        let loc: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let confidence: FieldRef<Fake, f64> = FieldRef::new("confidence");
        let agg = loc
            .convex_hull()
            .filter(confidence.as_expr().gt(Expr::literal(0.5_f64)));
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
        let sql = acc.sql().to_string();
        assert!(
            sql.starts_with("ST_ConvexHull(ST_Collect("),
            "must open ST_ConvexHull(ST_Collect(...; got: {sql}"
        );
        assert!(
            sql.contains(" FILTER (WHERE confidence > "),
            "FILTER clause must be present; got: {sql}"
        );
        assert!(
            sql.ends_with(")::geography"),
            "must end with )::geography; got: {sql}"
        );
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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
    fn centroid_with_over_in_annotate_path_places_over_on_inner_collect() {
        // Codex T22 round-3 BLOCK-1 + round-4: when a spatial
        // aggregate is emitted through the windowed-annotate path
        // (the default `OVER ()` for ungrouped annotate, or an
        // explicit `.over(|w| ...)` window spec), the OVER clause
        // must attach to the *aggregate*, not to a scalar wrapper.
        //
        // For centroid, ST_Collect IS the aggregate; ST_Centroid is
        // a scalar function that wraps the collected geometry set.
        // OVER must fall inside ST_Centroid, attached to ST_Collect:
        //
        //   correct: ST_Centroid(ST_Collect(<col>::geometry) OVER (...))::geography
        //   wrong:   (ST_Centroid(ST_Collect(<col>::geometry)) OVER (...))::geography
        //
        // The wrong shape attaches OVER to the ST_Centroid scalar
        // call, which Postgres rejects with "OVER specified, but
        // ST_Centroid is not a window function nor an aggregate
        // function".
        use crate::pg::accumulator::SqlAccumulator;
        let loc: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let agg = loc.centroid();
        let mut acc = SqlAccumulator::new("");
        crate::query::sql::emit_aggregate_with_window_and_cast(&mut acc, &agg.node)
            .expect("aggregate emission");
        let sql = acc.sql().to_string();
        assert_eq!(
            sql, "ST_Centroid(ST_Collect(location::geometry) OVER ())::geography",
            "OVER must attach to the inner ST_Collect aggregate, inside the ST_Centroid \
             scalar wrapper; got: {sql}"
        );
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn collect_with_over_in_annotate_path_places_over_inside_geography_cast() {
        // Unwrapped spatial — `loc.collect()` has no scalar wrapper,
        // so OVER and cast attach the canonical aggregate-with-OVER
        // way: `(AGG(...) OVER ())::cast`.
        use crate::pg::accumulator::SqlAccumulator;
        let loc: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let agg = loc.collect();
        let mut acc = SqlAccumulator::new("");
        crate::query::sql::emit_aggregate_with_window_and_cast(&mut acc, &agg.node)
            .expect("aggregate emission");
        let sql = acc.sql().to_string();
        assert_eq!(
            sql, "(ST_Collect(location::geometry) OVER ())::geography",
            "OVER must fall inside the ::geography cast; got: {sql}"
        );
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn collect_with_filter_and_over_attaches_modifiers_to_aggregate_call() {
        // Cluster E round-5 BLOCK-1 closure: the unwrapped + FILTER +
        // OVER combination must produce
        // `(AGG(...) FILTER (...) OVER (...))::cast` — both modifiers
        // attach directly to the aggregate call. Pre-fix the bare
        // emission's FILTER parens nested inside the windowed
        // emission's outer parens, giving `((AGG FILTER) OVER)::cast`,
        // which Postgres rejects because OVER attaches to a
        // parenthesized expression rather than the aggregate.
        use crate::expr::Expr;
        use crate::pg::accumulator::SqlAccumulator;
        let loc: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let confidence: FieldRef<Fake, f64> = FieldRef::new("confidence");
        let agg = loc
            .collect()
            .filter(confidence.as_expr().gt(Expr::literal(0.5_f64)));
        let mut acc = SqlAccumulator::new("");
        crate::query::sql::emit_aggregate_with_window_and_cast(&mut acc, &agg.node)
            .expect("aggregate emission");
        let sql = acc.sql().to_string();
        assert!(
            sql.starts_with("(ST_Collect(location::geometry) FILTER (WHERE confidence > "),
            "expected single outer paren before ST_Collect (no nested filter parens); got: {sql}"
        );
        assert!(sql.contains(" OVER ()"), "OVER must be present; got: {sql}");
        assert!(
            sql.ends_with(")::geography"),
            "must end with )::geography (cast outside (AGG FILTER OVER)); got: {sql}"
        );
        // Critical anti-regression: NO double parens at the start.
        // Post-fix the SQL must NOT begin with `((` — that would be
        // the round-4 broken shape.
        assert!(
            !sql.starts_with("(("),
            "must not start with `((` (round-5 BLOCK-1 anti-regression); got: {sql}"
        );
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn centroid_with_filter_and_over_places_both_inside_inner_collect() {
        // Combined FILTER + OVER under wrapped spatial cast — both
        // modifiers attach to the inner `ST_Collect` aggregate,
        // inside the `ST_Centroid` scalar wrapper. Canonical
        // Postgres order: `WRAP(AGG(...) FILTER (...) OVER (...))::cast`.
        use crate::expr::Expr;
        use crate::pg::accumulator::SqlAccumulator;
        let loc: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let confidence: FieldRef<Fake, f64> = FieldRef::new("confidence");
        let agg = loc
            .centroid()
            .filter(confidence.as_expr().gt(Expr::literal(0.5_f64)));
        let mut acc = SqlAccumulator::new("");
        crate::query::sql::emit_aggregate_with_window_and_cast(&mut acc, &agg.node)
            .expect("aggregate emission");
        let sql = acc.sql().to_string();
        assert!(
            sql.starts_with("ST_Centroid(ST_Collect("),
            "must open with ST_Centroid(ST_Collect(...; got: {sql}"
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
            "must end with )::geography (cast outside wrapper); got: {sql}"
        );
        // Modifier order inside the wrapper: FILTER < OVER < ::geography.
        let filter_idx = sql.find(" FILTER (").unwrap();
        let over_idx = sql.find(" OVER (").unwrap();
        let cast_idx = sql.rfind("::geography").unwrap();
        assert!(
            filter_idx < over_idx && over_idx < cast_idx,
            "modifier order must be FILTER < OVER < ::geography; got: {sql}"
        );
    }

    #[cfg(feature = "spatial")]
    #[test]
    fn convex_hull_with_over_places_over_on_inner_collect() {
        // Codex T22 round-4 BLOCK-3 / round-5 BLOCK-2: convex_hull
        // is a wrapped spatial aggregate (sibling of centroid).
        // After the round-5 migration it routes through the same
        // `AggOp::SpatialConvexHull` envelope as centroid, so the
        // wrapped OVER splice (place OVER inside the wrapper, not
        // around the whole expression) applies uniformly.
        //
        //   correct: ST_ConvexHull(ST_Collect(<col>::geometry) OVER (...))::geography
        //   wrong:   ST_ConvexHull(ST_Collect(<col>::geometry))::geography OVER (...)
        use crate::pg::accumulator::SqlAccumulator;
        let loc: FieldRef<Fake, GeoPoint> = FieldRef::new("location");
        let agg = loc.convex_hull();
        let mut acc = SqlAccumulator::new("");
        crate::query::sql::emit_aggregate_with_window_and_cast(&mut acc, &agg.node)
            .expect("aggregate emission");
        let sql = acc.sql().to_string();
        assert_eq!(
            sql, "ST_ConvexHull(ST_Collect(location::geometry) OVER ())::geography",
            "OVER must attach to the inner ST_Collect aggregate, inside the ST_ConvexHull \
             scalar wrapper; got: {sql}"
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, crate::query::SqlEmitContext::root())
            .expect("aggregate emission");
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

    #[test]
    fn into_filter_value_u64_widens_to_decimal() {
        // djogi#186 (Phase 8.5 v3 Cluster 2) — `u64` was previously
        // omitted from `IntoFilterValue` because `u64::MAX > i64::MAX`
        // makes the simple signed widening unsound. The fix uses
        // `rust_decimal::Decimal`, which Postgres bare `NUMERIC`
        // round-trips losslessly.
        match 0u64.into_filter_value() {
            FilterValue::Decimal(v) => assert_eq!(v, rust_decimal::Decimal::from(0u64)),
            other => panic!("expected Decimal, got {other:?}"),
        }
        match u64::MAX.into_filter_value() {
            FilterValue::Decimal(v) => assert_eq!(v, rust_decimal::Decimal::from(u64::MAX)),
            other => panic!("expected Decimal, got {other:?}"),
        }
    }
}
