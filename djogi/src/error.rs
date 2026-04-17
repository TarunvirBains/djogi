//! The single error type returned by every framework CRUD operation.
//!
//! `DjogiError` wraps the sources of failure that can occur when a `Model`
//! method runs: sqlx errors, expected-row-count violations, and ID generation
//! failures from `heeranjid-sqlx`. Keeping one error type at the public API
//! makes `?`-propagation ergonomic: user code calls `Post::get(&pool, id).await?`
//! and gets a `DjogiError` without having to juggle per-subsystem errors.
//!
//! The variants correspond 1:1 to the failure modes the generated CRUD impls
//! can produce:
//! - `Sqlx`           — raw database/driver failures (network, constraints, SQL).
//! - `NotFound`       — `.get()` / `.fetch_one()` saw zero rows.
//! - `MultipleObjects`— `.fetch_one()` saw more than one row.
//! - `IdGeneration`   — `generate_heerid` / `generate_ranjid` DB calls failed.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DjogiError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("row not found")]
    NotFound,

    #[error("expected exactly one row, found multiple")]
    MultipleObjects,

    #[error("id generation failed: {0}")]
    IdGeneration(#[from] heeranjid_sqlx::GenerateError),
}
