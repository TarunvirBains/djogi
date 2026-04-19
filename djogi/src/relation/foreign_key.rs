//! `ForeignKey<T>` — the many-to-one relation primitive.
//!
//! Stores only the target's PK until `.fetch()` (or a future
//! `prefetch`/`select_related` call) populates a cached `T`. The
//! wrapper round-trips through SQLx as the target's PK type — the
//! DB column is nothing but a PK-typed foreign key, and the runtime
//! carries no row data on the unresolved wrapper.
//!
//! # Why the unresolved and resolved shapes are separate types
//!
//! `ForeignKey<T>` is constructed wherever the user hands the framework
//! a foreign-key value: form data, a fresh `Vehicle` being inserted, a
//! `FromRow` decode of a plain `SELECT`. In those paths there is
//! deliberately no cached child — the struct fits in a register (it
//! holds only `T::Pk`) and stays `Copy` when the PK is `Copy`.
//!
//! `ForeignKeyResolved<T>` is the post-eager-load shape. Phase 3
//! Task 4 (`prefetch`) and Task 5 (`select_related`) produce these
//! after issuing the extra SELECT / LEFT JOIN, so the `Option<Box<T>>`
//! box only exists on rows that actually carry a cached child.
//!
//! Keeping the two separate avoids a common Django-ORM footgun: a
//! single relation type that *sometimes* carries a child leads to
//! ambiguous code ("did this `.fk.owner` hit the DB or not?"). Here
//! the type tells you: if it's `ForeignKey<T>`, it definitely hasn't;
//! if it's `ForeignKeyResolved<T>`, a prefetch ran and `resolved()` /
//! `expect_resolved()` are the API.
//!
//! # sqlx wiring
//!
//! Both `ForeignKey<T>` and `ForeignKeyResolved<T>` encode/decode as
//! `T::Pk`. `ForeignKey<T>` additionally implements `Type`/`Encode`/
//! `Decode` directly so it can appear in `FromRow`-generated code
//! and in `QueryBuilder::bind(...)` calls. `ForeignKeyResolved<T>`
//! is constructed by the prefetch layer internally; user code
//! receives it as the field type on a prefetched view struct, never
//! binds it back into another query.

use crate::model::Model;
use sqlx::{Database, Decode, Encode, Postgres, Type};
use std::marker::PhantomData;

/// Strongly-typed PK-only reference to a related model.
///
/// Transport-shaped: wraps just the target's PK. Holds no cached row
/// data — eager loading produces a [`ForeignKeyResolved<T>`] instead.
///
/// The `PhantomData<fn() -> T>` marker makes `ForeignKey<T>` covariant
/// in `T` without implying ownership of a `T` value. That shape is the
/// right variance for a "logical reference to a T" — it matches the
/// relationship between `&T` and its lifetime, not the relationship
/// between `Box<T>` and its heap-owned `T`.
pub struct ForeignKey<T: Model> {
    key: T::Pk,
    _target: PhantomData<fn() -> T>,
}

// Manual `Clone` — `#[derive(Clone)]` would add a `T: Clone` bound
// because of `PhantomData<fn() -> T>`. The wrapper only ever needs
// `T::Pk: Clone` to copy the key, and `T` is only used as a type-level
// tag, so we write the impl by hand.
impl<T: Model> Clone for ForeignKey<T> {
    fn clone(&self) -> Self {
        Self {
            key: self.key.clone(),
            _target: PhantomData,
        }
    }
}

impl<T: Model> Copy for ForeignKey<T> where T::Pk: Copy {}

impl<T: Model> PartialEq for ForeignKey<T>
where
    T::Pk: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl<T: Model> Eq for ForeignKey<T> where T::Pk: Eq {}

impl<T: Model> std::hash::Hash for ForeignKey<T>
where
    T::Pk: std::hash::Hash,
{
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

impl<T: Model> std::fmt::Debug for ForeignKey<T>
where
    T::Pk: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Print `ForeignKey<Owner>(42)`-style. Using the target's
        // `table_name()` rather than any `std::any::type_name` string
        // keeps the output stable across rustc versions and readable
        // in logs.
        write!(f, "ForeignKey<{}>({:?})", T::table_name(), self.key)
    }
}

impl<T: Model> ForeignKey<T> {
    /// Construct an unresolved foreign-key reference to `key`.
    ///
    /// This is the constructor user code calls when building a row for
    /// insert: `Vehicle { owner: ForeignKey::new(owner.id), ... }`.
    /// `FromRow` decode and `sqlx::Decode` both funnel through this.
    #[inline]
    pub fn new(key: T::Pk) -> Self {
        Self {
            key,
            _target: PhantomData,
        }
    }

    /// Return a clone of the target's primary key.
    ///
    /// Returns an owned value rather than `&T::Pk` so the common
    /// `HeerId` / `i32` / `RanjId` PK types can be threaded through
    /// query binders without a borrow dance. For very large
    /// hypothetical PK types a user can still borrow via field
    /// destructuring in `match` / `if let` patterns (where the key
    /// field is accessible to crate-private code).
    #[inline]
    pub fn key(&self) -> T::Pk
    where
        T::Pk: Clone,
    {
        self.key.clone()
    }

    /// Always `None` on the unresolved wrapper.
    ///
    /// Present as a method on both `ForeignKey` and `ForeignKeyResolved`
    /// so generic code that handles either shape can call `.resolved()`
    /// uniformly. Callers who need a cached child must go through
    /// `prefetch()` / `select_related()` and receive a
    /// [`ForeignKeyResolved<T>`].
    #[inline]
    pub fn resolved(&self) -> Option<&T> {
        None
    }

    /// Explicit single-relation fetch. Issues one `SELECT` against the
    /// caller's `DjogiContext` by deferring to `T::get`. Use for
    /// opportunistic resolution inside a handler where a prefetch was not
    /// arranged upstream; otherwise `prefetch`/`select_related` are the
    /// scalable options.
    pub async fn fetch(
        &self,
        ctx: &mut crate::context::DjogiContext,
    ) -> Result<T, crate::DjogiError>
    where
        T::Pk: Clone,
    {
        T::get(ctx, self.key.clone()).await
    }
}

// ---------------------------------------------------------------------------
// sqlx integration for `ForeignKey<T>` — round-trip as `T::Pk`.
// ---------------------------------------------------------------------------

impl<T: Model> Type<Postgres> for ForeignKey<T>
where
    T::Pk: Type<Postgres>,
{
    fn type_info() -> <Postgres as Database>::TypeInfo {
        <T::Pk as Type<Postgres>>::type_info()
    }

    fn compatible(ty: &<Postgres as Database>::TypeInfo) -> bool {
        <T::Pk as Type<Postgres>>::compatible(ty)
    }
}

impl<'q, T: Model> Encode<'q, Postgres> for ForeignKey<T>
where
    T::Pk: Encode<'q, Postgres>,
{
    fn encode_by_ref(
        &self,
        buf: &mut <Postgres as Database>::ArgumentBuffer<'q>,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        <T::Pk as Encode<'q, Postgres>>::encode_by_ref(&self.key, buf)
    }
}

impl<'r, T: Model> Decode<'r, Postgres> for ForeignKey<T>
where
    T::Pk: Decode<'r, Postgres>,
{
    fn decode(
        value: <Postgres as Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        <T::Pk as Decode<'r, Postgres>>::decode(value).map(ForeignKey::new)
    }
}

// ---------------------------------------------------------------------------
// Filter-API integration — `ForeignKey<T>` projects through the target PK.
// ---------------------------------------------------------------------------
//
// A reverse-FK accessor (Phase 3 Task 7) filters the source table by its
// FK column: `Vehicle::objects().filter(|f| f.owner_id().eq(
// ForeignKey::new(owner.id)))`. The closure `.eq` call infers `V =
// ForeignKey<Owner>` from the field-handle's declared type; to satisfy the
// `V: IntoFilterValue` bound on `FieldRef::eq` we forward into the
// target's PK projection. No new `FilterValue` discriminant is needed —
// the FK round-trips through the SQL layer as its PK type, which is
// exactly the binding the condition tree already knows how to emit.
//
// `T::Pk: Clone` matches the bound already required by `ForeignKey::key`
// (and by the `Clone` impl on the wrapper itself), so this impl adds no
// new capability constraints on concrete `T`.

impl<T: Model> crate::query::field::IntoFilterValue for ForeignKey<T>
where
    T::Pk: crate::query::field::IntoFilterValue + Clone,
{
    fn into_filter_value(self) -> crate::query::condition::FilterValue {
        self.key.into_filter_value()
    }
}

// ---------------------------------------------------------------------------
// Post-fetch / post-prefetch resolved wrapper.
// ---------------------------------------------------------------------------

/// Post-eager-load variant of [`ForeignKey<T>`] that carries a cached child.
///
/// Produced by `QuerySet::prefetch()` (Phase 3 Task 4) and
/// `QuerySet::select_related()` (Phase 3 Task 5). Never constructed by
/// user code directly — the `new` constructor is `pub(crate)` on purpose.
///
/// # Why `Option<Box<T>>`
///
/// Two independent reasons — nullability and sizing — drive the layout.
///
/// The `Option` models a LEFT JOIN miss: a nullable FK column
/// (e.g. `fuel_type_id: Option<ForeignKey<FuelType>>`) is `NULL` on the
/// parent row, or a filter on the join side excluded the child. Surfacing
/// that as `None` instead of an error lets permissive reads stay
/// ergonomic; callers who asserted a prefetch ran use
/// [`expect_resolved`](ForeignKeyResolved::expect_resolved) to fail
/// loudly instead.
///
/// The `Box` keeps the FK wrapper small and *constant-sized* regardless
/// of `T`. Without it, embedding `ForeignKeyResolved<Vehicle>` directly
/// in a parent struct would balloon the parent by `size_of::<Vehicle>()`
/// — every additional resolved FK on a parent row would compound that
/// cost. Boxing keeps each resolved wrapper at one pointer plus the PK,
/// so a row with several prefetched FKs stays compact in memory and in
/// the `Vec<Row>` the query layer hands back.
///
/// `Clone` and `Debug` are manual impls gated on `T::Pk: Clone`/`Debug`
/// and `T: Clone`/`Debug` respectively so the test stub `Dummy` (which
/// deliberately carries no auto-trait blanket impls) still compiles
/// the module, while production `#[model]` structs — which derive
/// `Debug` and `Clone` — pick up the impls transparently.
pub struct ForeignKeyResolved<T: Model> {
    key: T::Pk,
    child: Option<Box<T>>,
}

impl<T: Model + Clone> Clone for ForeignKeyResolved<T>
where
    T::Pk: Clone,
{
    fn clone(&self) -> Self {
        Self {
            key: self.key.clone(),
            child: self.child.clone(),
        }
    }
}

impl<T: Model + std::fmt::Debug> std::fmt::Debug for ForeignKeyResolved<T>
where
    T::Pk: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForeignKeyResolved")
            .field("key", &self.key)
            .field("child", &self.child)
            .finish()
    }
}

impl<T: Model> ForeignKeyResolved<T> {
    /// Crate-private constructor used by the prefetch / select_related
    /// implementations in Phase 3 Tasks 4 and 5. Not part of the public
    /// surface: resolved wrappers always originate from the query layer.
    ///
    /// `#[allow(dead_code)]` — Tasks 4 and 5 add the call sites; until
    /// then only `#[cfg(test)]` tests in this module reach this
    /// constructor, which is invisible to the default build's
    /// dead-code analysis.
    #[allow(dead_code)]
    pub(crate) fn new(key: T::Pk, child: Option<T>) -> Self {
        Self {
            key,
            child: child.map(Box::new),
        }
    }

    /// Borrow the target's primary key.
    #[inline]
    pub fn key(&self) -> &T::Pk {
        &self.key
    }

    /// Return the cached child if the eager-load attached one.
    ///
    /// Returns `None` for a LEFT JOIN miss or when the target row was
    /// filtered out of a prefetched subquery. The permissive path —
    /// use this when nullability is an expected business outcome, not
    /// a bug signal.
    #[inline]
    pub fn resolved(&self) -> Option<&T> {
        self.child.as_deref()
    }

    /// Strict variant of [`resolved`](Self::resolved) — fails loudly
    /// when the caller has asserted a prefetch / select_related ran
    /// but the cache turned up empty.
    ///
    /// `model` and `field` are threaded into the `DjogiError::RelationUnloaded`
    /// error so log lines can identify the offending relation. Phase 3
    /// callers pass these at the call site (e.g.
    /// `.expect_resolved("Vehicle", "owner_id")`); a future phase may
    /// wire them in automatically via macro expansion on the
    /// prefetched view struct.
    #[inline]
    pub fn expect_resolved(
        &self,
        model: &'static str,
        field: &'static str,
    ) -> Result<&T, crate::DjogiError> {
        self.child
            .as_deref()
            .ok_or_else(|| crate::DjogiError::relation_unloaded(model, field))
    }
}

// ---------------------------------------------------------------------------
// Unit tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DjogiError;
    use crate::types::HeerId;

    /// Stub `Model` impl so the tests can name a concrete `ForeignKey<Dummy>`
    /// type without pulling in `#[derive(Model)]` (which would create a
    /// circular crate dependency inside `djogi/` itself). None of these
    /// stub methods are ever called — the compile surface is what matters.
    ///
    /// `Debug` is derived so `Result::unwrap_err` works on
    /// `Result<&Dummy, DjogiError>`, and `Clone` is derived so the
    /// `ForeignKeyResolved::clone` path stays exercised.
    #[derive(Debug, Clone)]
    struct Dummy;
    impl crate::model::__sealed::Sealed for Dummy {}
    #[allow(clippy::manual_async_fn)]
    impl crate::model::Model for Dummy {
        type Pk = HeerId;
        type Fields = ();
        fn table_name() -> &'static str {
            "dummies"
        }
        fn pk_value(&self) -> &HeerId {
            unreachable!()
        }
        fn descriptor() -> &'static crate::descriptor::ModelDescriptor {
            unreachable!()
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: HeerId,
        ) -> impl std::future::Future<Output = Result<Self, DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), DjogiError>> + Send + 'ctx {
            async { unreachable!() }
        }
        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<Self, DjogiError>> + Send + 'ctx {
            async { unreachable!() }
        }
    }

    #[test]
    fn foreign_key_stores_target_pk() {
        let fk: ForeignKey<Dummy> = ForeignKey::new(HeerId::from_i64(42).unwrap());
        assert_eq!(fk.key(), HeerId::from_i64(42).unwrap());
    }

    #[test]
    fn foreign_key_resolved_always_none_on_unresolved_wrapper() {
        let fk: ForeignKey<Dummy> = ForeignKey::new(HeerId::from_i64(7).unwrap());
        assert!(fk.resolved().is_none());
    }

    #[test]
    fn foreign_key_is_copy_when_pk_is_copy() {
        // `HeerId` is `Copy`, so `ForeignKey<Dummy>` is too. This is a
        // compile-time check; the runtime assertion is just belt-and-braces.
        fn takes_copy<T: Copy>(_: T) {}
        let fk: ForeignKey<Dummy> = ForeignKey::new(HeerId::from_i64(1).unwrap());
        takes_copy(fk);
        // `fk` is still usable after the Copy — demonstrates the move-free shape.
        let _second = fk;
    }

    #[test]
    fn foreign_key_eq_compares_by_key() {
        let a: ForeignKey<Dummy> = ForeignKey::new(HeerId::from_i64(99).unwrap());
        let b: ForeignKey<Dummy> = ForeignKey::new(HeerId::from_i64(99).unwrap());
        let c: ForeignKey<Dummy> = ForeignKey::new(HeerId::from_i64(100).unwrap());
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn foreign_key_resolved_expect_resolved_err_on_missing() {
        let resolved: ForeignKeyResolved<Dummy> =
            ForeignKeyResolved::new(HeerId::from_i64(1).unwrap(), None);
        let err = resolved.expect_resolved("Vehicle", "owner_id").unwrap_err();
        let msg = format!("{err}");
        assert!(
            matches!(
                err,
                DjogiError::RelationUnloaded {
                    model: "Vehicle",
                    field: "owner_id"
                }
            ),
            "expected RelationUnloaded, got: {err:?}"
        );
        assert!(msg.contains("Vehicle"), "expected model name, got: {msg}");
        assert!(msg.contains("owner_id"), "expected field name, got: {msg}");
    }

    #[test]
    fn foreign_key_resolved_expect_resolved_ok_on_present() {
        let resolved: ForeignKeyResolved<Dummy> =
            ForeignKeyResolved::new(HeerId::from_i64(1).unwrap(), Some(Dummy));
        // `expect_resolved` returns `&Dummy`; we can't easily compare
        // `Dummy` (no `Eq`), so just confirm the `Ok` branch and that
        // `resolved()` returns the same cached reference.
        assert!(resolved.expect_resolved("M", "f").is_ok());
        assert!(resolved.resolved().is_some());
    }
}
