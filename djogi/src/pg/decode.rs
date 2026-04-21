//! Crate-private row-decode bridge for T2.
//!
//! # What
//!
//! [`FromPgRowBridge`] is a crate-private trait emitted by the `#[model]` macro
//! (via `djogi-macros/src/model/from_row.rs`) alongside the existing sqlx
//! `FromRow` impl. It provides a single method `__from_pg_row` that decodes a
//! `tokio_postgres::Row` into the implementing type by name-based column lookup.
//!
//! # T2 bridge purpose
//!
//! The T2 terminals (`query/terminal.rs`, `relation/prefetch.rs`) need to decode
//! `tokio_postgres::Row` into model types `T`, but they cannot use sqlx's
//! `FromRow` trait directly (sqlx rows and tokio-postgres rows are unrelated
//! types). Rather than introducing a full `FromPgRow` public trait (T3's job),
//! T2 introduces this crate-private bridge trait that the macro emits alongside
//! the existing sqlx impl.
//!
//! Generic code that needs to decode `T` from a `tokio_postgres::Row` bounds on
//! `T: FromPgRowBridge`. The macro emits the implementation using ordinal column
//! access matching the struct field order from `build_select`.
//!
//! # T3 migration
//!
//! T3 replaces this module with the public `FromPgRow` trait, removes the sqlx
//! `FromRow` emission, and converts all bounded sites from `FromPgRowBridge` to
//! `FromPgRow`. At that point this module is deleted.

/// Crate-private row-decode bridge trait.
///
/// Implemented by `#[model]`-annotated structs (via macro emission in
/// `from_row.rs`) for the T2 tokio-postgres decode path. Each field is decoded
/// from the `tokio_postgres::Row` by name, in struct-field order.
///
/// Do not implement this trait manually — only the `#[model]` macro emits it.
/// It will be replaced by the public `FromPgRow` trait in T3.
pub trait FromPgRowBridge: Sized {
    /// Decode `Self` from a `tokio_postgres::Row` by column name.
    ///
    /// Returns `Err(DjogiError::Decode(...))` if any column is missing or its
    /// wire type cannot be converted to the expected Rust type. This preserves
    /// the Phase 4 contract that every CRUD failure flows through
    /// [`DjogiError`](crate::DjogiError) rather than aborting the task via
    /// `panic!`.
    ///
    /// The column names match the struct field names by convention (snake_case),
    /// which is the same convention `build_select` / `build_select_joined` uses
    /// in the SQL emitter. An empty-prefix call decodes the bare unaliased
    /// columns; `select_related` prefixed decoding is handled by
    /// [`crate::relation::joined_row::FromJoinedRow`] instead.
    fn __from_pg_row(row: &tokio_postgres::Row) -> Result<Self, crate::DjogiError>;
}
