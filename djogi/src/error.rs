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
//! - `Sqlx` — raw database/driver failures (network, constraints, SQL). In T2
//!   this variant carries both `sqlx::Error` and `tokio_postgres::Error` (via
//!   `From` impls). T6 renames it to `Db`.
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

use thiserror::Error;

/// A newtype wrapping a boxed error for ID-generation failures.
///
/// T2 pre-renames the `DjogiError::IdGeneration` payload from
/// `heeranjid_sqlx::GenerateError` to this local newtype, removing the runtime
/// dependency on `heeranjid-sqlx`. The newtype wraps any boxed `Error + Send + Sync`
/// so future callers can supply HeeRanjID 0.2's postgres-codec errors without this
/// crate coupling to a specific error type. T6 decides the final variant name and
/// payload shape.
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
    /// Raw database or driver error. In T2 this carries `sqlx::Error` (via the
    /// `#[from]` impl) and also accepts `tokio_postgres::Error` (via a manual
    /// `From` impl below). T6 renames this variant to `Db`.
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

    /// ID generation failed.
    ///
    /// T2 pre-renames the payload from `heeranjid_sqlx::GenerateError` to the
    /// local `IdGenerationError` newtype, removing the runtime dependency on
    /// `heeranjid-sqlx`. T6 decides the final variant name and payload shape.
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
    ///
    /// T6 will rename this variant to `DjogiError::Db` (wrapping a
    /// driver-neutral `DbError`) as part of the broader error-surface
    /// rewrite.
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
    /// [`Sqlx`](DjogiError::Sqlx). The classifier
    /// [`is_lock_error`] keeps the variant boundary tight.
    #[error("lock conflict: {0}")]
    LockConflict(#[source] sqlx::Error),
}

/// Bridge: convert `tokio_postgres::Error` into `DjogiError`.
///
/// In T2, tokio-postgres errors flow into the existing `DjogiError::Sqlx` variant
/// via a protocol-level conversion. T6 renames the variant to `DjogiError::Db`
/// and removes the sqlx wrapper. The `LockConflict` variant detection is ported
/// to SQLSTATE comparison via `tokio_postgres::error::SqlState`.
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
    /// | [`Sqlx`](Self::Sqlx) with SQLSTATE `40001` / `40P01` / `55P03` | transient |
    /// | [`Sqlx`](Self::Sqlx) with any other SQLSTATE | terminal |
    /// | [`NotFound`](Self::NotFound) | terminal |
    /// | [`MultipleObjects`](Self::MultipleObjects) | terminal |
    /// | [`IdGeneration`](Self::IdGeneration) | terminal |
    /// | [`RelationUnloaded`](Self::RelationUnloaded) | terminal |
    /// | [`Decode`](Self::Decode) | terminal |
    /// | [`Serde`](Self::Serde) | terminal |
    /// | [`Validation`](Self::Validation) | terminal |
    /// | [`MissingIdempotencyKey`](Self::MissingIdempotencyKey) | terminal |
    /// | [`GoneAggregate`](Self::GoneAggregate) | terminal |
    ///
    /// The Sqlx row reflects the existing `is_lock_error`
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
            DjogiError::Sqlx(e) => is_lock_error(e),
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
/// `sqlx::DatabaseError::code()` returns `Option<Cow<'_, str>>`, so the
/// `.as_deref()` collapses `Cow::Owned` / `Cow::Borrowed` into a plain
/// `&str` the `matches!` arm can compare against literal codes.
pub(crate) fn is_lock_error(e: &sqlx::Error) -> bool {
    matches!(
        e.as_database_error().and_then(|db| db.code()).as_deref(),
        Some("40001") | Some("40P01") | Some("55P03")
    )
}

/// Return `true` if the tokio-postgres error carries a lock/serialization
/// SQLSTATE — the class of failures `retry_on_conflict()` is willing to
/// re-run the closure through.
///
/// Matches three SQLSTATEs using `tokio_postgres::error::SqlState` constants:
/// - `SqlState::SERIALIZATION_FAILURE` — `40001`
/// - `SqlState::DEADLOCK_DETECTED` — `40P01`
/// - `SqlState::LOCK_NOT_AVAILABLE` — `55P03`
pub(crate) fn is_pg_lock_error(e: &tokio_postgres::Error) -> bool {
    use tokio_postgres::error::SqlState;
    // `T_R_` is tokio-postgres's naming convention for SQLSTATE class 40
    // ("transaction rollback") and class 55 ("object not in prerequisite state")
    // codes that require transaction retry. Specifically:
    //   `T_R_SERIALIZATION_FAILURE` = SQLSTATE 40001 (class 40 / T_R = transaction rollback)
    //   `T_R_DEADLOCK_DETECTED`     = SQLSTATE 40P01 (class 40 / P01 = Postgres extension)
    //   `LOCK_NOT_AVAILABLE`         = SQLSTATE 55P03 (class 55 / P03, no T_R_ prefix)
    e.as_db_error()
        .map(|db| {
            let code = db.code();
            code == &SqlState::T_R_SERIALIZATION_FAILURE
                || code == &SqlState::T_R_DEADLOCK_DETECTED
                || code == &SqlState::LOCK_NOT_AVAILABLE
        })
        .unwrap_or(false)
}

/// Lower a raw `sqlx::Error` into either
/// [`DjogiError::LockConflict`] (for retryable SQLSTATEs 40001/40P01/
/// 55P03) or [`DjogiError::Sqlx`] (for everything else).
///
/// Every terminal that honours row locking — `select_for_update` /
/// `nowait` / `skip_locked` — runs its SELECT's error through this so
/// the caller can pattern-match on `LockConflict` without re-
/// classifying the SQLSTATE itself. Bulk methods that need retry
/// semantics (Task 7 `bulk_create` / `bulk_update` under
/// `retry_on_conflict`) use the same path.
///
/// Callers that don't care about the distinction can still use `?`:
/// `From<sqlx::Error> for DjogiError` returns
/// [`DjogiError::Sqlx`] verbatim, so unclassified error paths stay
/// identical to pre-Task-7 behaviour.
#[allow(dead_code)] // sqlx-based lock classifier; kept through T9 alongside the sqlx::Error variants.
pub(crate) fn map_lock_err(e: sqlx::Error) -> DjogiError {
    if is_lock_error(&e) {
        DjogiError::LockConflict(e)
    } else {
        DjogiError::Sqlx(e)
    }
}

/// Lower a `tokio_postgres::Error` into either `DjogiError::LockConflict`
/// (for retryable SQLSTATEs) or `DjogiError::Sqlx` (wrapping via protocol
/// conversion for everything else).
///
/// T6 renames `DjogiError::Sqlx` to `DjogiError::Db` and updates the
/// `LockConflict` payload type. For T2 the sqlx variant serves as the
/// catch-all container.
pub(crate) fn map_pg_err(e: tokio_postgres::Error) -> DjogiError {
    if is_pg_lock_error(&e) {
        // Lock conflict — wrap in the existing LockConflict variant. In T2
        // the payload is still sqlx::Error; we convert via the protocol-string
        // bridge. T6 changes the payload type.
        DjogiError::LockConflict(sqlx::Error::Protocol(e.to_string()))
    } else if let Some(db) = e.as_db_error() {
        // Database-level error — embed the SQLSTATE code in the protocol
        // message so callers (including integration tests) can inspect it.
        // Format: "SQLSTATE:<code> <message>".
        // T6 replaces this with a proper DjogiError::Db(DbError) variant.
        let msg = format!("SQLSTATE:{} {}", db.code().code(), db.message());
        DjogiError::Sqlx(sqlx::Error::Protocol(msg))
    } else {
        // All other tokio-postgres errors go into the Sqlx variant in T2.
        // T6 renames this to DjogiError::Db(DbError).
        DjogiError::Sqlx(sqlx::Error::Protocol(e.to_string()))
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

    #[test]
    fn map_lock_err_lifts_retryable_sqlstates_into_lock_conflict() {
        // Every retryable SQLSTATE round-trips through `map_lock_err`
        // into the typed `LockConflict` variant. Callers can then
        // pattern-match on the variant without re-classifying the
        // sqlx error themselves.
        for code in ["40001", "40P01", "55P03"] {
            let mapped = map_lock_err(sqlx_err_with_code(code));
            assert!(
                matches!(mapped, DjogiError::LockConflict(_)),
                "SQLSTATE {code} should produce LockConflict, got {mapped:?}"
            );
        }
    }

    #[test]
    fn map_lock_err_passes_unrelated_sqlstate_through_as_sqlx() {
        // Non-lock SQLSTATEs (e.g. `23505` unique_violation) fall
        // through into `DjogiError::Sqlx` unchanged — `map_lock_err`
        // must not over-classify.
        let mapped = map_lock_err(sqlx_err_with_code("23505"));
        assert!(
            matches!(mapped, DjogiError::Sqlx(_)),
            "non-lock SQLSTATE should produce Sqlx, got {mapped:?}"
        );
    }

    #[test]
    fn map_lock_err_passes_non_database_error_through_as_sqlx() {
        // Errors that carry no DatabaseError (connection drops,
        // protocol errors, `RowNotFound`) have no SQLSTATE to inspect
        // and must fall through to `Sqlx` — never `LockConflict`.
        let mapped = map_lock_err(sqlx::Error::RowNotFound);
        assert!(
            matches!(mapped, DjogiError::Sqlx(_)),
            "non-database error should produce Sqlx, got {mapped:?}"
        );
    }

    #[test]
    fn is_transient_covers_lock_conflict_and_retryable_sqlx() {
        // LockConflict — the typed variant that map_lock_err lifts —
        // is always transient.
        let lc = DjogiError::LockConflict(sqlx_err_with_code("55P03"));
        assert!(lc.is_transient(), "LockConflict must be transient");
        assert!(!lc.is_terminal(), "LockConflict must not be terminal");

        // Raw Sqlx with retryable SQLSTATE — transient because
        // `is_lock_error` reports it so. Covers escape-hatch paths
        // that didn't route through map_lock_err.
        for code in ["40001", "40P01", "55P03"] {
            let err = DjogiError::Sqlx(sqlx_err_with_code(code));
            assert!(
                err.is_transient(),
                "Sqlx with SQLSTATE {code} must be transient"
            );
        }

        // Raw Sqlx with non-lock SQLSTATE — terminal.
        let unique = DjogiError::Sqlx(sqlx_err_with_code("23505"));
        assert!(unique.is_terminal(), "unique_violation must be terminal");
        assert!(
            !unique.is_transient(),
            "unique_violation must not be transient"
        );
    }

    #[test]
    fn is_terminal_covers_every_known_variant() {
        // Every non-Sqlx, non-LockConflict variant is terminal. Spell
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
