//! The `Model` trait — the contract every `#[model]` struct satisfies.
//!
//! All CRUD methods are generic over `sqlx::Executor` — the same call
//! works with `&pool` (auto-connection) or `&mut txn` (transaction).
//!
//! ## Executor lifetime parameter
//!
//! Each method introduces an explicit lifetime `'a` for the `sqlx::Executor`
//! bound. Rust 1.81+ RPITIT requires named lifetimes here; `'_` is unstable
//! in this position. At the trait level `'a` only scopes the parameter's
//! borrow — the trait has no bodies to capture it. Generated impls (Task 7)
//! `async move` the executor into their future, which is sound because
//! `sqlx::Executor: Send` propagates to the captured type.
//!
//! ## Send bounds
//!
//! `sqlx::Executor` already declares `Send` as a supertrait, so `+ Send` on
//! the executor parameter is redundant and omitted. The `Future` return types
//! carry `+ Send` explicitly so callers can `.await` them across task
//! boundaries.

use crate::DjogiError;
use crate::descriptor::ModelDescriptor;
use std::future::Future;

pub trait Model: Sized + Send + Sync + 'static {
    /// Primary key Rust type.
    /// - `pk = "heerid"` (default) → `HeerId`
    /// - `pk = "serial"` → `i32`
    /// - `pk = "ranjid"` → `uuid::Uuid`
    /// - `pk = "none"` → `()` — a `get()` impl is still generated to satisfy
    ///   the trait, but its body panics; it is not intended to be called.
    type Pk: Clone
        + Send
        + Sync
        + for<'q> sqlx::Encode<'q, sqlx::Postgres>
        + sqlx::Type<sqlx::Postgres>
        + 'static;

    /// SQL table name.
    fn table_name() -> &'static str;

    /// Returns the primary key value for this instance.
    fn pk_value(&self) -> &Self::Pk;

    /// Static model descriptor — used by the migration differ (Phase 6).
    fn descriptor() -> &'static ModelDescriptor;

    /// Fetch by primary key. Returns `DjogiError::NotFound` if absent.
    fn get<'a>(
        executor: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
        id: Self::Pk,
    ) -> impl Future<Output = Result<Self, DjogiError>> + Send;

    /// Insert a new row. Framework fields (`id`, `created_at`, `updated_at`)
    /// from `value` are ignored — the database populates them via defaults
    /// and `RETURNING *`.
    fn create<'a>(
        executor: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
        value: Self,
    ) -> impl Future<Output = Result<Self, DjogiError>> + Send;

    /// Update all user-defined fields for this row. Sets `updated_at = now()`.
    fn save<'a>(
        &self,
        executor: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
    ) -> impl Future<Output = Result<(), DjogiError>> + Send;

    /// Delete this row.
    fn delete<'a>(
        self,
        executor: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
    ) -> impl Future<Output = Result<(), DjogiError>> + Send;

    /// Reload this row from the database, returning a fresh instance.
    fn refresh_from_db<'a>(
        &self,
        executor: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
    ) -> impl Future<Output = Result<Self, DjogiError>> + Send;
}
