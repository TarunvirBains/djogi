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
//! - `Sqlx` — raw database/driver failures (network, constraints, SQL).
//! - `NotFound` — `.get()` / `.fetch_one()` saw zero rows; carries the
//!   offending table name for observability.
//! - `MultipleObjects` — `.fetch_one()` saw more than one row; carries the
//!   table name plus the actual count observed.
//! - `IdGeneration` — `generate_heerid` / `generate_ranjid` DB calls failed.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DjogiError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),

    /// `Model::get` / `QuerySet::fetch_one` saw zero rows. The `table` field
    /// records the SQL table that was queried so log lines and error reports
    /// remain meaningful once errors propagate far from the call site.
    #[error("row not found in `{table}`")]
    NotFound { table: &'static str },

    /// `QuerySet::fetch_one` (or similar) saw more than one row when exactly
    /// one was expected. `count_seen` is the number actually observed — with
    /// the `LIMIT 2` strategy this is always exactly `2`, but storing the real
    /// count keeps the error meaningful for future code paths that may
    /// pre-count rows differently.
    #[error("multiple objects returned from `{table}` (saw {count_seen}, expected exactly 1)")]
    MultipleObjects {
        table: &'static str,
        count_seen: usize,
    },

    #[error("id generation failed: {0}")]
    IdGeneration(#[from] heeranjid_sqlx::GenerateError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_displays_table_name() {
        let err = DjogiError::NotFound { table: "posts" };
        let msg = format!("{err}");
        assert!(
            msg.contains("posts"),
            "expected table name in error message, got: {msg}"
        );
        assert!(msg.to_lowercase().contains("not found"));
    }

    #[test]
    fn multiple_objects_displays_table_and_count() {
        let err = DjogiError::MultipleObjects {
            table: "posts",
            count_seen: 2,
        };
        let msg = format!("{err}");
        assert!(msg.contains("posts"));
        assert!(msg.contains("2"));
        assert!(msg.to_lowercase().contains("multiple"));
    }
}
