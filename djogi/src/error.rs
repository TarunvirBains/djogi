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
//! - `RelationUnloaded` — a relation accessor (`ForeignKeyResolved::expect_resolved`
//!   / `OneToOneFieldResolved::expect_resolved`) was invoked against a cache
//!   that was never populated. Raised from the strict path where the caller
//!   has asserted a `prefetch()` / `select_related()` happened upstream.
//!
//! # `#[non_exhaustive]` on the enum *and* its struct variants
//!
//! Both `DjogiError` and its struct-form variants (`NotFound`,
//! `MultipleObjects`) are marked `#[non_exhaustive]`. This is a deliberate
//! forward-compatibility choice:
//!
//! - **Enum-level.** Downstream matches MUST include a wildcard arm, so
//!   introducing a new variant in a future release is not a breaking change.
//!   Djogi is pre-publish today, but Phase 2+ adds filter-layer errors (type
//!   coercion, invalid operator) that will live here.
//! - **Variant-level.** Downstream destructuring patterns MUST use `..`, and
//!   struct-expression *construction* from outside this crate is blocked —
//!   that is exactly the desired shape for an error type. The only legitimate
//!   construction sites are inside djogi and inside `#[model]`-expanded code
//!   (which runs in user crates). The expanded code goes through the public
//!   constructors below (`DjogiError::not_found`, `DjogiError::multiple_objects`),
//!   matching the pattern established by `std::io::Error`, `hyper::Error`,
//!   and similar well-designed error types.
//!
//! The cost is one extra line of implementation (the constructor) and one
//! extra pair of dots at downstream destructuring sites. The benefit is
//! that adding a field to either struct variant is also non-breaking.

use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DjogiError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),

    /// `Model::get` / `QuerySet::fetch_one` saw zero rows. The `table` field
    /// records the SQL table that was queried so log lines and error reports
    /// remain meaningful once errors propagate far from the call site.
    #[error("row not found in `{table}`")]
    #[non_exhaustive]
    NotFound { table: &'static str },

    /// `QuerySet::fetch_one` (or similar) saw more than one row when exactly
    /// one was expected. `count_seen` is the number actually observed — with
    /// the `LIMIT 2` strategy this is always exactly `2`, but storing the real
    /// count keeps the error meaningful for future code paths that may
    /// pre-count rows differently.
    #[error("multiple objects returned from `{table}` (saw {count_seen}, expected exactly 1)")]
    #[non_exhaustive]
    MultipleObjects {
        table: &'static str,
        count_seen: usize,
    },

    #[error("id generation failed: {0}")]
    IdGeneration(#[from] heeranjid_sqlx::GenerateError),

    /// A relation accessor that requires an eagerly-loaded cache
    /// (`ForeignKeyResolved::expect_resolved` / `OneToOneFieldResolved::expect_resolved`)
    /// was invoked against a wrapper whose cache is empty. The caller
    /// asserted a `prefetch()` / `select_related()` ran earlier but none
    /// did — this is a strict-mode user error, not a query failure.
    ///
    /// `model` is the source model name (e.g. `"Vehicle"`), `field` is the
    /// relation field on that model (e.g. `"owner_id"`). Both are compile-time
    /// `&'static str`s — the macro fills them in from the struct definition
    /// in Phase 3 Task 2. Until then, callers supply them at the call site.
    #[error(
        "relation field `{field}` on `{model}` was not loaded — \
         use .prefetch() or .select_related() before .expect_resolved()"
    )]
    #[non_exhaustive]
    RelationUnloaded {
        model: &'static str,
        field: &'static str,
    },
}

impl DjogiError {
    /// Construct a `NotFound` error with a table-name context.
    ///
    /// This is the public escape hatch for `#[non_exhaustive]` on the
    /// `NotFound` variant: struct-expression construction is blocked outside
    /// this crate, so `#[model]`-expanded CRUD methods (which run in user
    /// crates) call this constructor instead. Keep the signature stable —
    /// any future additional context fields must gain their own constructor
    /// or builder rather than changing this one.
    pub fn not_found(table: &'static str) -> Self {
        DjogiError::NotFound { table }
    }

    /// Construct a `MultipleObjects` error with a table name and the number
    /// of rows actually observed.
    ///
    /// Mirror of `not_found` — exists so that cross-crate callers (macro
    /// output, future filter-layer builders) can produce this variant
    /// without running into `#[non_exhaustive]`.
    pub fn multiple_objects(table: &'static str, count_seen: usize) -> Self {
        DjogiError::MultipleObjects { table, count_seen }
    }

    /// Construct a `RelationUnloaded` error naming the model and relation
    /// field that the caller asked to resolve without loading.
    ///
    /// Exists for the same reason as `not_found` / `multiple_objects`: the
    /// `#[non_exhaustive]` attribute on the variant blocks struct-expression
    /// construction outside this crate, so macro-expanded code and Phase 3+
    /// relation wrappers go through this constructor instead.
    pub fn relation_unloaded(model: &'static str, field: &'static str) -> Self {
        DjogiError::RelationUnloaded { model, field }
    }
}

/// Return `true` if the sqlx error wraps a Postgres lock/serialization
/// conflict — the class of failures that `retry_on_conflict()` is
/// willing to re-run the closure through.
///
/// Matches three SQLSTATEs:
///
/// - `40001` (`serialization_failure`) — the classic MVCC serialization
///   error on `SERIALIZABLE`/`REPEATABLE READ` isolation.
/// - `40P01` (`deadlock_detected`) — Postgres detected a circular wait
///   and aborted one of the participants.
/// - `55P03` (`lock_not_available`) — a `NOWAIT` lock request could not
///   acquire its lock immediately.
///
/// The full `DjogiError::LockConflict` variant is deferred to Phase 4
/// Task 7 (row locks + bulk methods); this helper lands first so
/// `atomic()` / `retry_on_conflict()` in Task 1 can classify retryable
/// errors without waiting on the error-type refactor.
///
/// `sqlx::DatabaseError::code()` returns `Option<Cow<'_, str>>`, so the
/// `.as_deref()` collapses `Cow::Owned` / `Cow::Borrowed` into a plain
/// `&str` the `matches!` arm can compare against literal codes.
pub(crate) fn is_lock_error(e: &sqlx::Error) -> bool {
    matches!(
        e.as_database_error().and_then(|db| db.code()).as_deref(),
        Some("40001") | Some("40P01") | Some("55P03")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_displays_table_name() {
        let err = DjogiError::not_found("posts");
        let msg = format!("{err}");
        assert!(
            msg.contains("posts"),
            "expected table name in error message, got: {msg}"
        );
        assert!(msg.to_lowercase().contains("not found"));
    }

    #[test]
    fn multiple_objects_displays_table_and_count() {
        let err = DjogiError::multiple_objects("posts", 2);
        let msg = format!("{err}");
        assert!(msg.contains("posts"));
        assert!(msg.contains("2"));
        assert!(msg.to_lowercase().contains("multiple"));
    }

    #[test]
    fn relation_unloaded_displays_model_and_field() {
        let err = DjogiError::relation_unloaded("Vehicle", "owner_id");
        let msg = format!("{err}");
        assert!(msg.contains("Vehicle"), "expected model name, got: {msg}");
        assert!(msg.contains("owner_id"), "expected field name, got: {msg}");
        assert!(
            msg.contains("prefetch") || msg.contains("select_related"),
            "expected remediation hint, got: {msg}"
        );
    }

    /// Minimal `sqlx::error::DatabaseError` stub for `is_lock_error`
    /// classification tests. Only the `code()` path matters for
    /// classification; all other methods return placeholder values
    /// because `is_lock_error` never invokes them.
    #[derive(Debug)]
    struct StubDbError {
        code: Option<String>,
    }

    impl std::fmt::Display for StubDbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "stub db error code={:?}", self.code)
        }
    }

    impl std::error::Error for StubDbError {}

    impl sqlx::error::DatabaseError for StubDbError {
        fn message(&self) -> &str {
            "stub"
        }
        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            self.code.as_deref().map(std::borrow::Cow::Borrowed)
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    fn sqlx_err_with_code(code: &str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(StubDbError {
            code: Some(code.to_string()),
        }))
    }

    #[test]
    fn is_lock_error_matches_retryable_sqlstates() {
        assert!(is_lock_error(&sqlx_err_with_code("40001")));
        assert!(is_lock_error(&sqlx_err_with_code("40P01")));
        assert!(is_lock_error(&sqlx_err_with_code("55P03")));
    }

    #[test]
    fn is_lock_error_rejects_unrelated_sqlstate() {
        // `23505` is `unique_violation` — a real error, but not one
        // `retry_on_conflict` should retry. Classifying it as non-retryable
        // proves the match arm is tight.
        assert!(!is_lock_error(&sqlx_err_with_code("23505")));
    }

    #[test]
    fn is_lock_error_rejects_non_database_error() {
        // `RowNotFound` has no underlying DatabaseError, so `.code()` is
        // `None` and the match fails.
        assert!(!is_lock_error(&sqlx::Error::RowNotFound));
    }
}
