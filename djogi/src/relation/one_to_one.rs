//! `OneToOneField<T>` — a unique-constrained singular relation.
//!
//! Reuses the `ForeignKey<T>` runtime wire-shape exactly. The
//! difference lives in descriptor metadata (the `RelationKind::OneToOne`
//! flag Phase 3 Task 2 records) and in the DDL the migration layer
//! emits in Phase 6 (`UNIQUE` on the relation column plus the reverse
//! side of the relation generating a singular accessor rather than a
//! `Vec<T>`). Runtime CRUD behavior is identical to `ForeignKey`.
//!
//! # Why a newtype rather than a type alias
//!
//! A type alias (`type OneToOneField<T> = ForeignKey<T>`) would be
//! shorter but would collapse the two at every use site — the macro
//! could not distinguish `OneToOneField<T>` from `ForeignKey<T>` when
//! scanning struct fields, and the public API could not evolve the
//! two shapes independently (e.g. if we later give `OneToOneField`
//! a distinct `reverse()` singular accessor). The newtype keeps the
//! identities separate at compile time with a single field of
//! overhead (the inner `ForeignKey<T>` is the only runtime state).

use crate::model::Model;
use crate::relation::foreign_key::{ForeignKey, ForeignKeyResolved};

/// Unique-constrained 1:1 relation field.
///
/// Public API of a thin newtype over [`ForeignKey<T>`] — same encode,
/// same decode, same `fetch` / `resolved` shape. See the module-level
/// docs for the rationale.
pub struct OneToOneField<T: Model>(ForeignKey<T>);

// Manual `Clone`, same reason as `ForeignKey<T>` — avoid a phantom
// `T: Clone` bound that `#[derive(Clone)]` would add.
impl<T: Model> Clone for OneToOneField<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T: Model> Copy for OneToOneField<T> where T::Pk: Copy {}

impl<T: Model> PartialEq for OneToOneField<T>
where
    T::Pk: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl<T: Model> Eq for OneToOneField<T> where T::Pk: Eq {}

impl<T: Model> std::hash::Hash for OneToOneField<T>
where
    T::Pk: std::hash::Hash,
{
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl<T: Model> std::fmt::Debug for OneToOneField<T>
where
    T::Pk: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Distinguishable from `ForeignKey<T>`'s Debug so log lines tell
        // operators which relation flavor they're looking at.
        write!(f, "OneToOneField<{}>({:?})", T::table_name(), self.0.key())
    }
}

impl<T: Model> OneToOneField<T> {
    /// Construct an unresolved 1:1 reference to `key`.
    #[inline]
    pub fn new(key: T::Pk) -> Self {
        Self(ForeignKey::new(key))
    }

    /// Clone of the target's primary key.
    #[inline]
    pub fn key(&self) -> T::Pk
    where
        T::Pk: Clone,
    {
        self.0.key()
    }

    /// Always `None` on the unresolved wrapper — mirrors
    /// [`ForeignKey::resolved`]. Use prefetch / select_related to get
    /// a [`OneToOneFieldResolved<T>`] that can carry a cached child.
    #[inline]
    pub fn resolved(&self) -> Option<&T> {
        self.0.resolved()
    }

    /// Explicit single-relation fetch. Forwards to
    /// [`ForeignKey::fetch`] — identical wire behavior.
    pub async fn fetch<'a, E>(&self, executor: E) -> Result<T, crate::DjogiError>
    where
        E: sqlx::Executor<'a, Database = sqlx::Postgres>,
        T::Pk: Clone,
    {
        self.0.fetch(executor).await
    }
}

// ---------------------------------------------------------------------------
// sqlx integration — forward through the inner `ForeignKey<T>`.
// ---------------------------------------------------------------------------

impl<T: Model> sqlx::Type<sqlx::Postgres> for OneToOneField<T>
where
    T::Pk: sqlx::Type<sqlx::Postgres>,
{
    fn type_info() -> <sqlx::Postgres as sqlx::Database>::TypeInfo {
        <ForeignKey<T> as sqlx::Type<sqlx::Postgres>>::type_info()
    }

    fn compatible(ty: &<sqlx::Postgres as sqlx::Database>::TypeInfo) -> bool {
        <ForeignKey<T> as sqlx::Type<sqlx::Postgres>>::compatible(ty)
    }
}

impl<'q, T: Model> sqlx::Encode<'q, sqlx::Postgres> for OneToOneField<T>
where
    T::Pk: sqlx::Encode<'q, sqlx::Postgres>,
{
    fn encode_by_ref(
        &self,
        buf: &mut <sqlx::Postgres as sqlx::Database>::ArgumentBuffer<'q>,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        <ForeignKey<T> as sqlx::Encode<'q, sqlx::Postgres>>::encode_by_ref(&self.0, buf)
    }
}

impl<'r, T: Model> sqlx::Decode<'r, sqlx::Postgres> for OneToOneField<T>
where
    T::Pk: sqlx::Decode<'r, sqlx::Postgres>,
{
    fn decode(
        value: <sqlx::Postgres as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        <ForeignKey<T> as sqlx::Decode<'r, sqlx::Postgres>>::decode(value).map(OneToOneField)
    }
}

// ---------------------------------------------------------------------------
// Post-fetch / post-prefetch resolved wrapper — mirrors `ForeignKeyResolved`.
// ---------------------------------------------------------------------------

/// Post-eager-load variant of [`OneToOneField<T>`].
///
/// Produced by `prefetch()` / `select_related()`, never constructed by
/// user code. The `Option<Box<T>>` lives inside the wrapped
/// [`ForeignKeyResolved<T>`]; `expect_resolved` forwards through with
/// the same "strict mode" semantics.
///
/// `Clone` and `Debug` are hand-rolled rather than derived so the bounds
/// match the inner `ForeignKeyResolved<T>` precisely (`T::Pk: Clone` +
/// `T: Clone` for Clone; `T::Pk: Debug` + `T: Debug` for Debug). A
/// plain `#[derive]` here would add a redundant `T: Clone` / `T: Debug`
/// bound on the *newtype* that's already implied through the inner
/// type, but the derive machinery emits it with the wrong shape and
/// fails to compile.
pub struct OneToOneFieldResolved<T: Model>(ForeignKeyResolved<T>);

impl<T: Model + Clone> Clone for OneToOneFieldResolved<T>
where
    T::Pk: Clone,
{
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: Model + std::fmt::Debug> std::fmt::Debug for OneToOneFieldResolved<T>
where
    T::Pk: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("OneToOneFieldResolved")
            .field(&self.0)
            .finish()
    }
}

impl<T: Model> OneToOneFieldResolved<T> {
    /// Crate-private constructor, same ownership story as
    /// [`ForeignKeyResolved::new`]. `#[allow(dead_code)]` for the
    /// same Task-4/5 timing reason.
    #[allow(dead_code)]
    pub(crate) fn new(key: T::Pk, child: Option<T>) -> Self {
        Self(ForeignKeyResolved::new(key, child))
    }

    /// Borrow the target's primary key.
    #[inline]
    pub fn key(&self) -> &T::Pk {
        self.0.key()
    }

    /// Permissive accessor — returns `Some(&T)` when the prefetch /
    /// select_related attached a child, `None` otherwise.
    #[inline]
    pub fn resolved(&self) -> Option<&T> {
        self.0.resolved()
    }

    /// Strict accessor — see [`ForeignKeyResolved::expect_resolved`]
    /// for the full contract. This impl forwards through the inner
    /// resolved wrapper so the error variant and message format stay
    /// identical across relation flavors.
    #[inline]
    pub fn expect_resolved(
        &self,
        model: &'static str,
        field: &'static str,
    ) -> Result<&T, crate::DjogiError> {
        self.0.expect_resolved(model, field)
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

    /// Same stub pattern as `foreign_key.rs`. Duplicated here so each
    /// module's tests stay self-contained and one file's compile
    /// failure doesn't cascade. `Debug + Clone` derived for the same
    /// `unwrap_err` reason spelled out in `foreign_key.rs`.
    #[derive(Debug, Clone)]
    struct Dummy;
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
        fn get<'a>(
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
            _id: HeerId,
        ) -> impl std::future::Future<Output = Result<Self, DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create<'a>(
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'a>(
            &self,
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
        ) -> impl std::future::Future<Output = Result<(), DjogiError>> + Send {
            async { unreachable!() }
        }
        fn delete<'a>(
            self,
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
        ) -> impl std::future::Future<Output = Result<(), DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'a>(
            &self,
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
        ) -> impl std::future::Future<Output = Result<Self, DjogiError>> + Send {
            async { unreachable!() }
        }
    }

    #[test]
    fn one_to_one_field_stores_and_returns_key() {
        let o: OneToOneField<Dummy> = OneToOneField::new(HeerId::from_i64(77).unwrap());
        assert_eq!(o.key(), HeerId::from_i64(77).unwrap());
        assert!(o.resolved().is_none());
    }

    #[test]
    fn one_to_one_field_is_copy_when_pk_copy() {
        fn takes_copy<T: Copy>(_: T) {}
        let o: OneToOneField<Dummy> = OneToOneField::new(HeerId::from_i64(1).unwrap());
        takes_copy(o);
        let _again = o;
    }

    #[test]
    fn one_to_one_field_resolved_expect_resolved_err_on_missing() {
        let r: OneToOneFieldResolved<Dummy> =
            OneToOneFieldResolved::new(HeerId::from_i64(1).unwrap(), None);
        let err = r.expect_resolved("Profile", "user_id").unwrap_err();
        assert!(matches!(
            err,
            DjogiError::RelationUnloaded {
                model: "Profile",
                field: "user_id"
            }
        ));
    }

    #[test]
    fn one_to_one_field_resolved_expect_resolved_ok_on_present() {
        // Parity with `foreign_key_resolved_expect_resolved_ok_on_present` —
        // confirms the newtype forwards the `Ok(&T)` branch through the
        // inner `ForeignKeyResolved<T>` without altering semantics.
        let r: OneToOneFieldResolved<Dummy> =
            OneToOneFieldResolved::new(HeerId::from_i64(1).unwrap(), Some(Dummy));
        assert!(r.expect_resolved("Profile", "user_id").is_ok());
        assert!(r.resolved().is_some());
    }

    #[test]
    fn one_to_one_field_resolved_clone() {
        // Exercise the manual `Clone` impl on `OneToOneFieldResolved<T>`.
        // Both original and clone must keep the cached child reachable —
        // a bug that dropped the `Box<T>` on clone would show up here.
        let r: OneToOneFieldResolved<Dummy> =
            OneToOneFieldResolved::new(HeerId::from_i64(5).unwrap(), Some(Dummy));
        let r2 = r.clone();
        assert!(r.resolved().is_some());
        assert!(r2.resolved().is_some());
        assert_eq!(r.key(), r2.key());
    }

    #[test]
    fn one_to_one_field_resolved_key_borrow() {
        // `key()` returns `&T::Pk` — a borrow, not an owned clone.
        // Confirm the borrow matches the stored value.
        let pk = HeerId::from_i64(123).unwrap();
        let r: OneToOneFieldResolved<Dummy> = OneToOneFieldResolved::new(pk, None);
        assert_eq!(r.key(), &pk);
    }
}
