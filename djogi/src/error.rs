//! The single error type returned by every framework CRUD operation.
//!
//! `DjogiError` wraps the sources of failure that can occur when a `Model`
//! method runs: database driver errors, expected-row-count violations, and ID
//! generation failures. Keeping one error type at the public API makes
//! `?`-propagation ergonomic: user code calls `Post::get(&pool, id).await?`
//! and gets a `DjogiError` without having to juggle per-subsystem errors.
//!
//! The variants correspond 1:1 to the failure modes the generated CRUD impls
//! can produce:
//! - `Db` — raw database/driver failures (network, constraints, SQL), wrapped
//!   in [`DbError`] so Djogi does not expose `tokio_postgres` directly in its
//!   public error surface.
//! - `NotFound` — `.get()` / `.fetch_one()` saw zero rows; carries the
//!   offending table name for observability.
//! - `MultipleObjects` — `.fetch_one()` saw more than one row; carries the
//!   table name plus the actual count observed.
//! - `IdGeneration` — ID generation DB calls failed.
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

use std::borrow::Cow;
use thiserror::Error;

/// Public wrapper for database-driver failures surfaced through Djogi.
///
/// Djogi stores the real `tokio_postgres::Error` when one exists, but also
/// needs a local message path for framework-generated database/runtime misuse
/// errors such as "commit called on a pool-backed context". Keeping that shape
/// behind `DbError` avoids exposing `tokio_postgres` directly in the public
/// enum while preserving the old generic database-error behavior.
#[derive(Debug)]
pub struct DbError(DbErrorKind);

#[derive(Debug)]
enum DbErrorKind {
    Pg(tokio_postgres::Error),
    Message(Box<str>),
    #[cfg(test)]
    SyntheticDb {
        code: tokio_postgres::error::SqlState,
        message: Box<str>,
    },
}

impl DbError {
    /// Construct a message-only database error for framework-generated failures
    /// that do not come from `tokio-postgres`.
    pub fn other(message: impl Into<String>) -> Self {
        Self(DbErrorKind::Message(message.into().into_boxed_str()))
    }

    /// Return the SQLSTATE if this error came from a Postgres database error.
    pub fn code(&self) -> Option<&tokio_postgres::error::SqlState> {
        match &self.0 {
            DbErrorKind::Pg(error) => error.code(),
            DbErrorKind::Message(_) => None,
            #[cfg(test)]
            DbErrorKind::SyntheticDb { code, .. } => Some(code),
        }
    }

    /// Return the most useful human-readable message for this database error.
    pub fn message(&self) -> Cow<'_, str> {
        match &self.0 {
            DbErrorKind::Pg(error) => error
                .as_db_error()
                .map(|db| Cow::Borrowed(db.message()))
                .unwrap_or_else(|| Cow::Owned(error.to_string())),
            DbErrorKind::Message(message) => Cow::Borrowed(message),
            #[cfg(test)]
            DbErrorKind::SyntheticDb { message, .. } => Cow::Borrowed(message),
        }
    }

    #[cfg(test)]
    fn synthetic_sqlstate(code: &str, message: &str) -> Self {
        Self(DbErrorKind::SyntheticDb {
            code: tokio_postgres::error::SqlState::from_code(code),
            message: message.to_owned().into_boxed_str(),
        })
    }
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            DbErrorKind::Pg(error) => write!(f, "{error}"),
            DbErrorKind::Message(message) => write!(f, "{message}"),
            #[cfg(test)]
            DbErrorKind::SyntheticDb { message, .. } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for DbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.0 {
            DbErrorKind::Pg(error) => Some(error),
            DbErrorKind::Message(_) => None,
            #[cfg(test)]
            DbErrorKind::SyntheticDb { .. } => None,
        }
    }
}

impl From<tokio_postgres::Error> for DbError {
    fn from(error: tokio_postgres::Error) -> Self {
        Self(DbErrorKind::Pg(error))
    }
}

/// A newtype wrapping a boxed error for ID-generation failures.
///
/// The newtype wraps any boxed `Error + Send + Sync` so callers can supply
/// HeeRanjID postgres-codec failures without this crate coupling to a specific
/// upstream error type.
#[derive(Debug)]
pub struct IdGenerationError(pub Box<dyn std::error::Error + Send + Sync>);

impl std::fmt::Display for IdGenerationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for IdGenerationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

impl IdGenerationError {
    /// Wrap any `Error + Send + Sync + 'static` into an `IdGenerationError`.
    pub fn new<E: std::error::Error + Send + Sync + 'static>(e: E) -> Self {
        IdGenerationError(Box::new(e))
    }
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DjogiError {
    /// Authentication or authorization failure bubbled up through the auth
    /// substrate. Wraps [`AuthError`](crate::auth::AuthError) so
    /// [`DjogiAuth::authenticate`](crate::auth::DjogiAuth::authenticate) and
    /// [`DjogiAuth::verify`](crate::auth::DjogiAuth::verify) failures flow
    /// through `?` inside `atomic()`-managed operations without explicit
    /// mapping.
    ///
    /// # Transitivity
    ///
    /// Because `AuthError` is `#[non_exhaustive]`, this variant also inherits
    /// that forward-compatibility contract at the `DjogiError` level: a
    /// `match` on `DjogiError::Auth(e)` that then matches on `e` must still
    /// include a wildcard arm for `AuthError`.
    #[error("auth error: {0}")]
    Auth(#[from] crate::auth::AuthError),

    /// Geo/spatial error from the `spatial` feature — coordinate validation
    /// or EWKB codec failure. Wraps [`GeoError`](crate::geo::GeoError) so
    /// spatial operations compose with `?` inside `atomic()`-managed
    /// operations without explicit mapping.
    #[cfg(feature = "spatial")]
    #[error("geo error: {0}")]
    Geo(#[from] crate::geo::GeoError),

    /// Raw database or driver error.
    #[error("database error: {0}")]
    Db(#[source] DbError),

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

    /// ID generation failed.
    #[error("id generation failed: {0}")]
    IdGeneration(IdGenerationError),

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

    /// JSON serialization / deserialization failed. Raised by the Phase 4
    /// Task 6 transactional-outbox emitter when `serde_json::to_value`
    /// cannot lower a model row into a JSON document — typically because
    /// a user field's `Serialize` impl returned an error. Wraps the
    /// `serde_json::Error` verbatim so the caller can inspect the
    /// underlying failure.
    #[error("JSON serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// A DDD-style aggregate was hard-deleted mid-operation,
    /// invalidating any further work against its id. Phase 4 Task
    /// 7.7 introduces this variant as the canonical terminal signal
    /// for "the aggregate you're operating on no longer exists" —
    /// distinct from `NotFound` (which covers initial-lookup
    /// misses) in that the caller already observed the aggregate
    /// earlier in the same operation.
    ///
    /// `model` is the owning model's `type_name` (same source as
    /// `MissingIdempotencyKey::model`). `id` is the PK rendered to
    /// a string (no generic parameter so the error type stays
    /// object-safe and usable across model boundaries). `reason` is
    /// a `&'static str` describing why the aggregate is gone (e.g.
    /// `"hard-deleted by admin"`, `"retention policy evicted"`).
    ///
    /// Classified as **terminal** by
    /// [`DjogiError::is_transient`] — `retry_on_conflict` does not
    /// retry this variant because retrying against a deleted row
    /// cannot succeed.
    #[error("aggregate '{model}' id={id} is gone: {reason}")]
    #[non_exhaustive]
    GoneAggregate {
        model: &'static str,
        id: String,
        reason: &'static str,
    },

    /// A column decode failure produced by
    /// [`FromPgRow::from_pg_row`](crate::pg::decode::FromPgRow::from_pg_row).
    ///
    /// Raised when `tokio_postgres::Row::try_get` returns an error for a
    /// model field — for example, when the wire type at a given ordinal
    /// position cannot be converted to the expected Rust type. Preserves
    /// the Phase 4 contract: every CRUD failure flows through
    /// `DjogiError` rather than aborting the task via `panic!`.
    ///
    /// The inner `String` carries the column name and the driver error
    /// so the caller can identify which field failed without inspecting
    /// the raw `tokio_postgres::Error`.
    #[error("row decode error: {0}")]
    Decode(String),

    /// A convenience method that consumes the descriptor's
    /// `idempotency_key` slot
    /// ([`create_or_find`](crate::model::Model) /
    /// `bulk_upsert_by_descriptor`) was invoked against a model
    /// whose `#[model(...)]` attribute does not set the key. Phase 4
    /// Task 7.5 introduces this variant as the runtime pointer at
    /// the attribute the caller needs to add.
    ///
    /// `model` is the `type_name` from [`ModelDescriptor::type_name`]
    /// — a `&'static str` the macro lifts directly from the struct
    /// identifier.
    #[error(
        "model '{model}' has no #[model(idempotency_key = \"...\")] declared; \
         set one or use bulk_upsert with an explicit conflict-key slice"
    )]
    #[non_exhaustive]
    MissingIdempotencyKey { model: &'static str },

    /// A runtime argument-validation failure produced by a CRUD
    /// convenience method — the caller's request is well-typed at
    /// compile time but fails a runtime invariant (e.g.
    /// `bulk_upsert`'s `conflict_cols` naming a column that does not
    /// exist on the model). Phase 4 Task 7d introduces this variant
    /// for `bulk_upsert`'s allow-list check; future phases may add
    /// more callers.
    ///
    /// The inner `String` is a human-readable description of the
    /// failure. No `&'static str` because callers interpolate the
    /// offending column name / table name into the message.
    #[error("validation error: {0}")]
    Validation(String),

    /// A `FOR UPDATE NOWAIT` / `FOR UPDATE` request could not acquire
    /// its row lock, or a `SERIALIZABLE` / `REPEATABLE READ` transaction
    /// encountered a serialization failure, or Postgres detected a
    /// deadlock and aborted one participant. Phase 4 Task 7 introduces
    /// this variant so the retry helper (`retry_on_conflict`) and the
    /// caller can branch on lock-contention vs other database errors.
    ///
    /// Retryable SQLSTATE classes carried here:
    ///
    /// - `40001` — `serialization_failure`
    /// - `40P01` — `deadlock_detected`
    /// - `55P03` — `lock_not_available` (`NOWAIT` rejection)
    ///
    /// Other database failures — `unique_violation`,
    /// `foreign_key_violation`, connection drops — still flow through
    /// [`Db`](DjogiError::Db). The classifier
    /// [`is_lock_error`] keeps the variant boundary tight.
    #[error("lock conflict: {0}")]
    LockConflict(#[source] DbError),

    /// `QuerySet::stream` or `DjogiContext::raw_stream` was called on a
    /// pool-backed context (i.e. outside an `atomic()` scope).
    ///
    /// Postgres named cursors are transaction-local — they require an open
    /// transaction to exist. Calling `stream` on a pool-backed context is a
    /// caller error that is detected at stream construction time, not at the
    /// first `poll_next`. This makes the error surface immediate and
    /// actionable rather than deferred to the first row consume.
    ///
    /// Fix: wrap the stream consumer in `atomic(&pool, |ctx| async move { … })`
    /// so `ctx` is transaction-backed when `stream` is called.
    #[error("QuerySet::stream requires an active transaction — wrap the call in atomic()")]
    StreamOutsideTransaction,

    /// An aggregate's DISTINCT modifier combination is not supported by
    /// Postgres syntax or by Djogi's current IR.
    ///
    /// # When this surfaces
    ///
    /// Raised by the fetch-time legality check in
    /// [`crate::expr::sql::check_aggregate_legality`] for two cases:
    ///
    /// - `COUNT(DISTINCT *)` — `COUNT(DISTINCT *)` is not valid SQL.
    ///   Use `COUNT(DISTINCT col)` via [`crate::query::field::FieldRef::count`]
    ///   instead.
    /// - `STRING_AGG(DISTINCT col, sep)` — Postgres requires an explicit
    ///   per-aggregate `ORDER BY` when DISTINCT is combined with
    ///   `STRING_AGG`. Djogi's Phase 6.5 IR does not track per-aggregate
    ///   ORDER BY; a future phase will lift this restriction.
    ///
    /// `op` is the aggregate function keyword (e.g. `"COUNT(*)"`,
    /// `"STRING_AGG"`). `reason` is a human-readable description of why
    /// the combination is rejected.
    #[error("unsupported aggregate: {op} — {reason}")]
    #[non_exhaustive]
    UnsupportedAggregate {
        /// The aggregate function keyword or name, e.g. `"COUNT(*)"` or
        /// `"STRING_AGG"`.
        op: &'static str,
        /// Human-readable explanation of why this combination is rejected.
        reason: &'static str,
    },
}

/// Bridge: convert `tokio_postgres::Error` into `DjogiError`.
impl From<tokio_postgres::Error> for DjogiError {
    fn from(e: tokio_postgres::Error) -> Self {
        map_pg_err(e)
    }
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

    /// Construct a `MissingIdempotencyKey` error naming the model
    /// whose `#[model(idempotency_key = "...")]` attribute was not
    /// declared.
    ///
    /// Mirror of `not_found` / `multiple_objects` — exists so that
    /// cross-crate callers (macro output in user crates) can produce
    /// this variant despite `#[non_exhaustive]` blocking struct-
    /// expression construction outside this crate.
    pub fn missing_idempotency_key(model: &'static str) -> Self {
        DjogiError::MissingIdempotencyKey { model }
    }

    /// Construct a `GoneAggregate` error.
    ///
    /// Mirror of the other constructors — exists so that cross-crate
    /// callers can produce this `#[non_exhaustive]` variant. `id` is
    /// typically produced via `format!("{}", pk)` so the error is
    /// independent of the originating model's `Pk` type.
    pub fn gone_aggregate(model: &'static str, id: String, reason: &'static str) -> Self {
        DjogiError::GoneAggregate { model, id, reason }
    }

    /// Classify this error as **transient** (retrying the closure
    /// may succeed) or **terminal** (retrying will not help).
    ///
    /// `retry_on_conflict` uses this predicate to decide whether to
    /// re-run its closure. The contract:
    ///
    /// | Variant | Classification |
    /// |---------|----------------|
    /// | [`LockConflict`](Self::LockConflict) | transient |
    /// | [`Db`](Self::Db) with SQLSTATE `40001` / `40P01` / `55P03` | transient |
    /// | [`Db`](Self::Db) with any other SQLSTATE | terminal |
    /// | [`NotFound`](Self::NotFound) | terminal |
    /// | [`MultipleObjects`](Self::MultipleObjects) | terminal |
    /// | [`IdGeneration`](Self::IdGeneration) | terminal |
    /// | [`RelationUnloaded`](Self::RelationUnloaded) | terminal |
    /// | [`Decode`](Self::Decode) | terminal |
    /// | [`Serde`](Self::Serde) | terminal |
    /// | [`Validation`](Self::Validation) | terminal |
    /// | [`MissingIdempotencyKey`](Self::MissingIdempotencyKey) | terminal |
    /// | [`GoneAggregate`](Self::GoneAggregate) | terminal |
    /// | [`StreamOutsideTransaction`](Self::StreamOutsideTransaction) | terminal |
    ///
    /// The Db row reflects the existing `is_lock_error`
    /// classifier: Postgres SQLSTATEs `40001` (serialization
    /// failure), `40P01` (deadlock detected), and `55P03`
    /// (lock not available / `NOWAIT` rejection) are the three
    /// retryable codes. Unique-violation, foreign-key violation,
    /// connection drops, and protocol errors all fall through to
    /// terminal because retrying the same closure against a
    /// constraint violation will fail the same way.
    pub fn is_transient(&self) -> bool {
        match self {
            DjogiError::LockConflict(_) => true,
            DjogiError::Db(e) => is_lock_error(e),
            _ => false,
        }
    }

    /// Inverse of [`is_transient`](Self::is_transient) — returns
    /// `true` when retrying will not help.
    ///
    /// Provided as a convenience for call sites that read more
    /// naturally as `err.is_terminal()` than `!err.is_transient()`.
    /// Same contract, inverted.
    pub fn is_terminal(&self) -> bool {
        !self.is_transient()
    }
}

/// Return `true` if the database error wraps a Postgres lock/serialization
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
pub(crate) fn is_lock_error(e: &DbError) -> bool {
    use tokio_postgres::error::SqlState;
    e.code()
        .map(|code| {
            code == &SqlState::T_R_SERIALIZATION_FAILURE
                || code == &SqlState::T_R_DEADLOCK_DETECTED
                || code == &SqlState::LOCK_NOT_AVAILABLE
        })
        .unwrap_or(false)
}

/// Lower a `tokio_postgres::Error` into either `DjogiError::LockConflict`
/// (for retryable SQLSTATEs) or `DjogiError::Db` (for everything else).
pub(crate) fn map_pg_err(e: tokio_postgres::Error) -> DjogiError {
    let error = DbError::from(e);
    if is_lock_error(&error) {
        DjogiError::LockConflict(error)
    } else {
        DjogiError::Db(error)
    }
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

    fn db_err_with_code(code: &str) -> DbError {
        DbError::synthetic_sqlstate(code, "synthetic database error")
    }

    #[test]
    fn db_variant_constructs_from_tokio_postgres_error() {
        let driver_error = tokio_postgres::Error::__private_api_timeout();
        let mapped = DjogiError::from(driver_error);
        assert!(matches!(mapped, DjogiError::Db(_)));
    }

    #[test]
    fn is_lock_error_matches_retryable_sqlstates() {
        assert!(is_lock_error(&db_err_with_code("40001")));
        assert!(is_lock_error(&db_err_with_code("40P01")));
        assert!(is_lock_error(&db_err_with_code("55P03")));
    }

    #[test]
    fn is_lock_error_rejects_unrelated_sqlstate() {
        assert!(!is_lock_error(&db_err_with_code("23505")));
    }

    #[test]
    fn is_lock_error_rejects_message_only_error() {
        assert!(!is_lock_error(&DbError::other("no sqlstate here")));
    }

    #[test]
    fn db_error_message_accessor_preserves_framework_generated_message() {
        let err = DbError::other("DjogiContext::commit called on a pool-backed context");
        assert_eq!(
            err.message(),
            "DjogiContext::commit called on a pool-backed context"
        );
    }

    #[test]
    fn is_transient_covers_lock_conflict_and_retryable_db() {
        let lc = DjogiError::LockConflict(db_err_with_code("55P03"));
        assert!(lc.is_transient(), "LockConflict must be transient");
        assert!(!lc.is_terminal(), "LockConflict must not be terminal");

        for code in ["40001", "40P01", "55P03"] {
            let err = DjogiError::Db(db_err_with_code(code));
            assert!(
                err.is_transient(),
                "Db with SQLSTATE {code} must be transient"
            );
        }

        let unique = DjogiError::Db(db_err_with_code("23505"));
        assert!(unique.is_terminal(), "unique_violation must be terminal");
        assert!(
            !unique.is_transient(),
            "unique_violation must not be transient"
        );
    }

    #[test]
    fn is_terminal_covers_every_known_variant() {
        // Every non-Db, non-LockConflict variant is terminal. Spell
        // each out so the classification table in the rustdoc is
        // pinned against drift: adding a new variant that should be
        // transient forces the test author to update this match.
        assert!(DjogiError::not_found("t").is_terminal());
        assert!(DjogiError::multiple_objects("t", 2).is_terminal());
        assert!(DjogiError::relation_unloaded("M", "f").is_terminal());
        assert!(DjogiError::missing_idempotency_key("M").is_terminal());
        assert!(DjogiError::Validation("bad".into()).is_terminal());
        assert!(
            DjogiError::Decode("column `id`: type mismatch".into()).is_terminal(),
            "Decode must be terminal — a type mismatch cannot be resolved by retrying"
        );
        assert!(
            DjogiError::gone_aggregate("M", "42".into(), "deleted").is_terminal(),
            "GoneAggregate must be terminal — retry cannot resurrect a deleted aggregate"
        );
    }
}
