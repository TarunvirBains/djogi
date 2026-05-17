//! `ForeignKey<T>` — the many-to-one relation primitive.
//!
//! Stores only the target's PK until `.fetch()` (or a future
//! `prefetch`/`select_related` call) populates a cached `T`. The
//! wrapper round-trips through postgres_types as the target's PK type — the
//! DB column is nothing but a PK-typed foreign key, and the runtime
//! carries no row data on the unresolved wrapper.
//!
//! # Why the unresolved and resolved shapes are separate types
//!
//! `ForeignKey<T>` is constructed wherever the user hands the framework
//! a foreign-key value: form data, a fresh `Vehicle` being inserted, a
//! row decode of a plain `SELECT`. In those paths there is
//! deliberately no cached child — the struct fits in a register (it
//! holds only `T::Pk`) and stays `Copy` when the PK is `Copy`.
//!
//! `ForeignKeyResolved<T>` is the post-eager-load shape. Phase 3
//! Task 4 (`prefetch`) and Task 5 (`select_related`) produce these
//! after issuing the extra SELECT / LEFT JOIN, so the `Option<Box<T>>`
//! box only exists on rows that actually carry a cached child.
//!
//! Keeping the two separate avoids a common ORM footgun: a
//! single relation type that *sometimes* carries a child leads to
//! ambiguous code ("did this `.fk.owner` hit the DB or not?"). Here
//! the type tells you: if it's `ForeignKey<T>`, it definitely hasn't;
//! if it's `ForeignKeyResolved<T>`, a prefetch ran and `resolved()` /
//! `expect_resolved()` are the API.
//!
//! # postgres_types wiring
//!
//! Both `ForeignKey<T>` and `ForeignKeyResolved<T>` encode/decode as
//! `T::Pk`. `ForeignKey<T>` additionally implements `ToSql`/`FromSql`
//! directly so it can appear in row decode and in bind-parameter
//! arrays. `ForeignKeyResolved<T>` is constructed by the prefetch layer
//! internally; user code receives it as the field type on a prefetched
//! view struct, never binds it back into another query.
//!
//! Row decode is handled by the macro-emitted
//! [`FromPgRow::from_pg_row`](crate::pg::decode::FromPgRow::from_pg_row) impl
//! which calls `row.try_get(i)` positionally. `ForeignKey<T>` is decoded
//! through its `postgres_types::FromSql` impl below.

use crate::model::Model;
use bytes::BytesMut;
use postgres_types::{FromSql, IsNull, ToSql, Type};
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
    /// Row decode (via the `postgres_types::FromSql` impl below) funnels through this.
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
// postgres_types integration for `ForeignKey<T>` — round-trip as `T::Pk`.
// ---------------------------------------------------------------------------

impl<T: Model> ToSql for ForeignKey<T>
where
    T::Pk: ToSql,
{
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        <T::Pk as ToSql>::to_sql(&self.key, ty, out)
    }

    fn accepts(ty: &Type) -> bool {
        <T::Pk as ToSql>::accepts(ty)
    }

    postgres_types::to_sql_checked!();
}

impl<'a, T: Model> FromSql<'a> for ForeignKey<T>
where
    T::Pk: FromSql<'a>,
{
    fn from_sql(
        ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        <T::Pk as FromSql<'a>>::from_sql(ty, raw).map(ForeignKey::new)
    }

    fn accepts(ty: &Type) -> bool {
        <T::Pk as FromSql<'a>>::accepts(ty)
    }
}

// Type encode/decode bridge impls that previously lived here existed solely
// to support an earlier macro-emitted row-decode path. T3 replaced that
// emission with `impl FromPgRow for T` (ordinal decode via
// `postgres_types::FromSql`), so those bridges are dead code and have been
// removed. `ForeignKey<T>` is now decoded entirely through its
// `postgres_types::FromSql` impl above.

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

// ---------------------------------------------------------------------------
// serde integration for `ForeignKey<T>` — round-trip as `T::Pk`. (#38)
// ---------------------------------------------------------------------------
//
// Without this, `#[model(events)]` cannot be enabled on entities with FK
// columns: the outbox emit path requires `T: Model + Serialize`, and the
// derived `Serialize` on a struct containing `ForeignKey<U>` fails because
// the wrapper itself does not satisfy the bound. Conceptually a
// `ForeignKey<U>` is just a typed handle around `U::Pk`, so serializing it
// as the wrapped `U::Pk` is the natural representation. The shape mirrors
// `Tracked<T>` (`djogi/src/tracked.rs`).
impl<T: Model> serde::Serialize for ForeignKey<T>
where
    T::Pk: serde::Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.key.serialize(serializer)
    }
}

impl<'de, T: Model> serde::Deserialize<'de> for ForeignKey<T>
where
    T::Pk: serde::Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        <T::Pk as serde::Deserialize<'de>>::deserialize(deserializer).map(ForeignKey::new)
    }
}

impl<T: Model> crate::query::field::IntoFilterValue for ForeignKey<T>
where
    T::Pk: crate::query::field::IntoFilterValue + Clone,
{
    fn into_filter_value(self) -> crate::query::condition::FilterValue {
        self.key.into_filter_value()
    }
}

impl<T: Model + 'static> crate::query::field::DjogiPortableEq for ForeignKey<T>
where
    T::Pk: crate::query::field::DjogiPortableEq,
{
}

// ---------------------------------------------------------------------------
// Expression-IR integration — `FieldRef<M, ForeignKey<T>>` ↔ `Expr<T::Pk>`.
// ---------------------------------------------------------------------------

impl<M: Model, T: Model> crate::query::field::FieldRef<M, ForeignKey<T>> {
    /// Promote this foreign-key column handle into an
    /// `Expr<T::Pk>` — the target PK's type — so it composes with
    /// [`crate::expr::OuterRef`] / [`crate::query::field::FieldRef`]
    /// handles on the target side inside a correlated subquery.
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn as_pk_expr(self) -> crate::expr::Expr<T::Pk> {
        crate::expr::Expr::from_node(crate::expr::node::ExprNode::Field {
            column: self.column(),
        })
    }
}

// PR3: post-flip root accessors return `DjogiField<M, ForeignKey<T>>` for
// FK columns. `as_pk_expr` is a non-predicate SQL helper (the result is
// `Expr<T::Pk>`, an expression-IR node consumed by builders like
// `correlated_subquery::*`), so it forwards to the wrapper's stored
// SQL handle without entering the portable predicate boundary. The
// macro-generated `m2m` / FK navigation emitters call this method
// inside `.filter_expr(|f| Expr::eq(f.fk().as_pk_expr(), …))`; routing
// through the wrapper keeps the same call site working post-PR3.
impl<M: Model, T: Model> crate::query::field::DjogiField<M, ForeignKey<T>> {
    /// Promote this foreign-key column handle into an
    /// `Expr<T::Pk>` — the target PK's type. Forwarded from
    /// [`FieldRef::as_pk_expr`](crate::query::field::FieldRef::as_pk_expr).
    /// PR3 makes this directly available on the post-flip
    /// `DjogiField` accessor surface so existing macro-emitted
    /// correlated-subquery patterns keep compiling.
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn as_pk_expr(self) -> crate::expr::Expr<T::Pk> {
        crate::expr::Expr::from_node(crate::expr::node::ExprNode::Field {
            column: self.column(),
        })
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
    /// implementations in Phase 3 Tasks 4 and 5.
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
    #[inline]
    pub fn resolved(&self) -> Option<&T> {
        self.child.as_deref()
    }

    /// Strict variant of [`resolved`](Self::resolved) — fails loudly
    /// when the caller has asserted a prefetch / select_related ran
    /// but the cache turned up empty.
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
    /// type without pulling in `#[derive(Model)]`.
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
            &'ctx mut self,
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
        fn takes_copy<T: Copy>(_: T) {}
        let fk: ForeignKey<Dummy> = ForeignKey::new(HeerId::from_i64(1).unwrap());
        takes_copy(fk);
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
        assert!(resolved.expect_resolved("M", "f").is_ok());
        assert!(resolved.resolved().is_some());
    }

    #[test]
    fn foreign_key_field_as_pk_expr_emits_bare_column() {
        // `FieldRef<M, ForeignKey<T>>::as_pk_expr()` must emit the same
        // bare column name that the default `as_expr()` does.
        use crate::expr::sql::emit_expr;
        use crate::pg::accumulator::SqlAccumulator;
        use crate::query::field::FieldRef;
        use crate::query::portable::SqlEmitContext;

        let fk_col: FieldRef<Dummy, ForeignKey<Dummy>> = FieldRef::new("ledger_id");
        let expr: crate::expr::Expr<HeerId> = fk_col.as_pk_expr();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &expr.node, SqlEmitContext::root()).expect("expression emission");
        assert_eq!(acc.sql().trim(), "ledger_id", "got: {}", acc.sql());
    }

    // GH issue #38 — `ForeignKey<T>` serializes as the wrapped `T::Pk` so
    // that `#[model(events)]` outbox emission compiles for entities with
    // FK columns. `HeerId` itself serializes as a JSON string (the i64
    // value rendered as decimal text) to preserve precision in JS clients,
    // so the round-trip below threads that contract end-to-end.
    #[test]
    fn foreign_key_serializes_as_wrapped_pk() {
        let fk: ForeignKey<Dummy> = ForeignKey::new(HeerId::from_i64(42).unwrap());
        let json = serde_json::to_string(&fk).expect("serialize");
        let pk_json = serde_json::to_string(&HeerId::from_i64(42).unwrap()).expect("pk serialize");
        assert_eq!(json, pk_json, "FK must serialize identically to its PK");
    }

    #[test]
    fn foreign_key_round_trips_through_json() {
        let fk: ForeignKey<Dummy> = ForeignKey::new(HeerId::from_i64(7).unwrap());
        let json = serde_json::to_string(&fk).expect("serialize");
        let restored: ForeignKey<Dummy> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored, fk);
    }
}
