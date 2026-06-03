//! The single error type returned by every framework CRUD operation.
//! `DjogiError` wraps the sources of failure that can occur when a `Model`
//! method runs: database driver errors, expected-row-count violations, and ID
//! generation failures. Keeping one error type at the public API makes
//! `?`-propagation ergonomic: user code calls `Post::get(&pool, id).await?`
//! and gets a `DjogiError` without having to juggle per-subsystem errors.
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
//! # `#[non_exhaustive]` on the enum *and* its struct variants
//! Both `DjogiError` and its struct-form variants (`NotFound`,
//! `MultipleObjects`) are marked `#[non_exhaustive]`. This is a deliberate
//! forward-compatibility choice:
//! - **Enum-level.** Downstream matches MUST include a wildcard arm, so
//!   introducing a new variant in a future release is not a breaking change.
//!   Djogi is pre-publish today, but + adds filter-layer errors (type
//!   coercion, invalid operator) that will live here.
//! - **Variant-level.** Downstream destructuring patterns MUST use `..`, and
//!   struct-expression *construction* from outside this crate is blocked
//!   that is exactly the desired shape for an error type. The only legitimate
//!   construction sites are inside djogi and inside `#[model]`-expanded code
//!   (which runs in user crates). The expanded code goes through the public
//!   constructors below (`DjogiError::not_found`, `DjogiError::multiple_objects`),
//!   matching the pattern established by `std::io::Error`, `hyper::Error`,
//!   and similar well-designed error types.
//!   The cost is one extra line of implementation (the constructor) and one
//!   extra pair of dots at downstream destructuring sites. The benefit is
//!   that adding a field to either struct variant is also non-breaking.

use std::borrow::Cow;
use thiserror::Error;

fn presentation_startup_error_summary(
    errors: &[crate::presentation::PresentationStartupError],
) -> String {
    if errors.is_empty() {
        return "no individual codec startup errors were collected".to_owned();
    }

    errors
        .iter()
        .enumerate()
        .map(|(idx, error)| {
            let msg = error.to_string();
            let msg = msg
                .strip_prefix("presentation codec startup: ")
                .unwrap_or(msg.as_str());
            format!("{}. {msg}", idx + 1)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Public wrapper for database-driver failures surfaced through Djogi.
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

/// The single error type returned by every Djogi CRUD operation.
/// Every fallible call site in djogi (`Model::create`, `QuerySet::fetch_all`,
/// `transaction::atomic`, `DjogiContext::set_tenant`, every raw escape hatch,
/// every spatial / FTS / JSONB helper) returns `Result<T, DjogiError>`. The
/// crate-scoped [`crate::Result<T>`] alias spells exactly that — adopter code
/// signs functions with `-> djogi::Result<T>` and uses `?` to propagate.
/// # Error taxonomy
/// `DjogiError` groups failures by where they originate, not by HTTP-style
/// status code. The most common branches:
/// - **Database-driver errors** — [`Db`](DjogiError::Db) wraps every
///   [`tokio_postgres::Error`] (network, constraints, syntax, auth) behind
///   the [`DbError`] facade so this enum does not leak `tokio_postgres` types.
/// - **Expected-row-count violations** — [`NotFound`](DjogiError::NotFound)
///   for zero rows from `Model::get` / `QuerySet::fetch_one`,
///   [`MultipleObjects`](DjogiError::MultipleObjects) for >1 row from
///   `fetch_one`. Both carry the offending table name.
/// - **Concurrency / contention** — [`LockConflict`](DjogiError::LockConflict)
///   for `40001` / `40P01` / `55P03` SQLSTATE classes (serialization
///   failures, deadlocks, `NOWAIT` rejections),
///   [`PoolTimeout`](DjogiError::PoolTimeout) for `deadpool` checkout
///   exhaustion. Both classify as transient — see
///   [`DjogiError::is_transient`].
/// - **Auth / RLS** — [`Auth`](DjogiError::Auth) wraps
///   [`AuthError`](crate::auth::AuthError) for authentication / authorization
///   failures from the auth substrate;
///   [`SetRoleOutsideTransaction`](DjogiError::SetRoleOutsideTransaction)
///   and [`InvalidRoleName`](DjogiError::InvalidRoleName) surface RLS-
///   overlay misuse.
/// - **Misuse / runtime invariants**
///   [`Validation`](DjogiError::Validation) for runtime argument validation
///   failures,
///   [`MissingIdempotencyKey`](DjogiError::MissingIdempotencyKey) for
///   upsert-attribute gaps,
///   [`StreamOutsideTransaction`](DjogiError::StreamOutsideTransaction) for
///   cursor / `QuerySet::stream` outside `atomic`,
///   [`UnsupportedAggregate`](DjogiError::UnsupportedAggregate) for IR /
///   Postgres aggregate mismatches.
/// - **Decode / serialization** — [`Decode`](DjogiError::Decode) for
///   `FromPgRow` failures, [`Serde`](DjogiError::Serde) for outbox JSON
///   serialization, [`Visage`](DjogiError::Visage) for visage-projection
///   `TryFrom<&Model>` failures.
/// - **ID generation** — [`IdGeneration`](DjogiError::IdGeneration) wraps
///   HeeRanjID-side codec failures.
/// - **Aggregate lifecycle** — [`GoneAggregate`](DjogiError::GoneAggregate)
///   for terminal "already gone" signals on a previously-observed
///   aggregate;
///   [`RelationUnloaded`](DjogiError::RelationUnloaded) for prefetch-cache
///   misses on a strict-mode resolved-relation accessor.
/// - **Spatial** — [`Geo`](DjogiError::Geo) (gated on the `spatial` feature)
///   wraps coordinate / EWKB codec errors.
/// # Retry classification
/// [`DjogiError::is_transient`] returns `true` for `LockConflict`,
/// `PoolTimeout`, and the small set of variants whose failures are expected
/// to be retryable. The framework also recognises the SQLSTATE classes that
/// indicate contention vs. other database failures — both classifications
/// are intended for use in caller-side retry policies (see also
/// [`retry_on_conflict`](crate::transaction::retry_on_conflict)).
/// # `#[non_exhaustive]`
/// Both `DjogiError` and its struct-form variants (`NotFound`,
/// `MultipleObjects`, `RelationUnloaded`, `GoneAggregate`,
/// `MissingIdempotencyKey`, `UnsupportedAggregate`, `PoolTimeout`,
/// `InvalidRoleName`, etc.) are `#[non_exhaustive]`. Downstream `match`
/// expressions MUST include a wildcard arm, and downstream destructuring
/// MUST use `..` — adding a new variant or new field to a struct variant
/// is therefore a non-breaking change. The cost is one extra wildcard arm
/// at every match site; the benefit is the framework can grow new failure
/// shapes (added a half-dozen) without breaking adopter code.
/// Construction from outside this crate is also blocked by the variant-level
/// `#[non_exhaustive]`. Use the public constructors
/// (`DjogiError::not_found`, `DjogiError::multiple_objects`, etc.) when
/// surfacing a Djogi-flavoured error from adopter code; this matches the
/// pattern set by `std::io::Error`, `hyper::Error`, and similar
/// well-designed error types.
/// # Why one error type
/// Every framework call returning `Result<T, DjogiError>` keeps `?`
/// propagation ergonomic across CRUD, raw SQL, transactions, auth, and
/// spatial/JSONB layers. Per-subsystem error types would force adopter
/// code into manual conversion at every layer boundary — exactly the
/// friction that drove the framework's "one error type at the public API"
/// design.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DjogiError {
    /// Authentication or authorization failure bubbled up through the auth
    /// substrate. Wraps [`AuthError`](crate::auth::AuthError) so
    /// [`DjogiAuth::authenticate`](crate::auth::DjogiAuth::authenticate) and
    /// [`DjogiAuth::verify`](crate::auth::DjogiAuth::verify) failures flow
    /// through `?` inside `atomic()`-managed operations without explicit
    /// mapping.
    /// # Transitivity
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
    /// `model` is the source model name (e.g. `"Vehicle"`), `field` is the
    /// relation field on that model (e.g. `"owner_id"`). Both are compile-time
    /// `&'static str`s — the macro fills them in from the struct definition
    /// in . Until then, callers supply them at the call site.
    #[error(
        "relation field `{field}` on `{model}` was not loaded — \
         use .prefetch() or .select_related() before .expect_resolved()"
    )]
    #[non_exhaustive]
    RelationUnloaded {
        model: &'static str,
        field: &'static str,
    },

    /// JSON serialization / deserialization failed. Raised by the
    /// Task 6 transactional-outbox emitter when `serde_json::to_value`
    /// cannot lower a model row into a JSON document — typically because
    /// a user field's `Serialize` impl returned an error. Wraps the
    /// `serde_json::Error` verbatim so the caller can inspect the
    /// underlying failure.
    #[error("JSON serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// Visage projection failure — raised when a `TryFrom<&Model>` impl on
    /// a generated visage cannot convert the row.
    /// Known triggers include
    /// [`VisageError::UnresolvedRelation`](crate::visage::VisageError), raised
    /// when a relation-nesting visage is projected from a model whose
    /// relation fields were not `prefetch()`-ed or `select_related()`-ed,
    /// and [`VisageError::PresentationCodec`](crate::visage::VisageError)
    /// from fallible protected-field presentation codecs.
    /// Introduces this variant so the visage-scoped
    /// reverse-FK / M2M accessors can flow a fallible peer-visage conversion
    /// through `?` without losing the VisageError structure. The
    /// `#[from]` shorthand on the inner type produces the
    /// `impl From<VisageError> for DjogiError` conversion the emitted
    /// accessors rely on.
    #[error("visage error: {0}")]
    Visage(#[from] crate::visage::VisageError),

    /// A DDD-style aggregate was hard-deleted mid-operation,
    /// invalidating any further work against its id. Task
    /// 7.7 introduces this variant as the canonical terminal signal
    /// for "the aggregate you're operating on no longer exists"
    /// distinct from `NotFound` (which covers initial-lookup
    /// misses) in that the caller already observed the aggregate
    /// earlier in the same operation.
    /// `model` is the owning model's `type_name` (same source as
    /// `MissingIdempotencyKey::model`). `id` is the PK rendered to
    /// a string (no generic parameter so the error type stays
    /// object-safe and usable across model boundaries). `reason` is
    /// a `&'static str` describing why the aggregate is gone (e.g.
    /// `"hard-deleted by admin"`, `"retention policy evicted"`).
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
    /// Raised when `tokio_postgres::Row::try_get` returns an error for a
    /// model field — for example, when the wire type at a given ordinal
    /// position cannot be converted to the expected Rust type. Preserves
    /// the contract: every CRUD failure flows through
    /// `DjogiError` rather than aborting the task via `panic!`.
    /// The inner `String` carries the column name and the driver error
    /// so the caller can identify which field failed without inspecting
    /// the raw `tokio_postgres::Error`.
    #[error("row decode error: {0}")]
    Decode(String),

    /// A convenience method that consumes the descriptor's
    /// `idempotency_key` slot
    /// ([`create_or_find`](crate::model::Model) /
    /// `bulk_upsert_by_descriptor`) was invoked against a model
    /// whose `#[model(...)]` attribute does not set the key.
    /// Task 7.5 introduces this variant as the runtime pointer at
    /// the attribute the caller needs to add.
    /// `model` is the `type_name` from [`ModelDescriptor::type_name`]
    /// a `&'static str` the macro lifts directly from the struct
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
    /// exist on the model). Task 7d introduces this variant
    /// for `bulk_upsert`'s allow-list check; future phases may add
    /// more callers.
    /// The inner `String` is a human-readable description of the
    /// failure. No `&'static str` because callers interpolate the
    /// offending column name / table name into the message.
    #[error("validation error: {0}")]
    Validation(String),

    /// A `FOR UPDATE NOWAIT` / `FOR UPDATE` request could not acquire
    /// its row lock, or a `SERIALIZABLE` / `REPEATABLE READ` transaction
    /// encountered a serialization failure, or Postgres detected a
    /// deadlock and aborted one participant. introduces
    /// this variant so the retry helper (`retry_on_conflict`) and the
    /// caller can branch on lock-contention vs other database errors.
    /// Retryable SQLSTATE classes carried here:
    /// - `40001` — `serialization_failure`
    /// - `40P01` — `deadlock_detected`
    /// - `55P03` — `lock_not_available` (`NOWAIT` rejection)
    ///   Other database failures — `unique_violation`,
    ///   `foreign_key_violation`, connection drops — still flow through
    ///   [`Db`](DjogiError::Db). The classifier
    ///   [`is_lock_error`] keeps the variant boundary tight.
    #[error("lock conflict: {0}")]
    LockConflict(#[source] DbError),

    /// `QuerySet::stream` or `DjogiContext::raw_stream` was called on a
    /// pool-backed context (i.e. outside an `atomic()` scope).
    /// Postgres named cursors are transaction-local — they require an open
    /// transaction to exist. Calling `stream` on a pool-backed context is a
    /// caller error that is detected at stream construction time, not at the
    /// first `poll_next`. This makes the error surface immediate and
    /// actionable rather than deferred to the first row consume.
    /// Fix: wrap the stream consumer in
    /// `atomic(&mut ctx, |ctx| Box::pin(async move { … }))` so `ctx` is
    /// transaction-backed when `stream` is called.
    #[error("QuerySet::stream requires an active transaction — wrap the call in atomic()")]
    StreamOutsideTransaction,

    /// A transaction-backed [`crate::DjogiContext`] was marked unsafe to
    /// continue after a nested `atomic()` future was dropped before the
    /// framework could run savepoint cleanup.
    /// Rust `Drop` cannot await `ROLLBACK TO SAVEPOINT` / `RELEASE
    /// SAVEPOINT`, so the safe contract is fail-closed: framework-owned
    /// operations reject further work, `commit` rolls the outer transaction
    /// back instead of committing it, and the caller must retry the outer unit
    /// of work from a fresh transaction.
    /// Classified as **terminal** by [`DjogiError::is_transient`] — retrying
    /// against the same poisoned context cannot make the transaction safe to
    /// commit.
    #[error(
        "transaction is poisoned ({reason}): a nested atomic future was dropped before \
         savepoint cleanup could run; the transaction is unsafe to commit, roll it back \
         and retry the outer unit of work"
    )]
    #[non_exhaustive]
    TransactionPoisoned {
        /// Static reason tag naming the poison source.
        reason: &'static str,
    },

    /// A transaction-backed raw SQL call attempted a session-scoped statement
    /// that `atomic()` cannot safely scrub on commit/rollback.
    /// This variant is used by the raw SQL bypass harness to reject
    /// session-level control statements such as plain `SET`, `RESET`,
    /// `LISTEN`, `UNLISTEN`, `PREPARE`, `DEALLOCATE`, and `DISCARD` when the
    /// context is already inside an `atomic()` transaction. Those statements
    /// either outlive the surrounding transaction entirely or invite callers
    /// to assume rollback will clean them up when Postgres semantics say
    /// otherwise.
    /// `statement` is the canonical top-level keyword (`"SET"`, `"RESET"`,
    /// etc.) that triggered the refusal. The fix is structural: use a
    /// transaction-local form such as `SET LOCAL` / `SET CONSTRAINTS` /
    /// `SET TRANSACTION`, or run the session-scoped statement on a pool-backed
    /// context outside the transaction.
    /// Classified as **terminal** by [`DjogiError::is_transient`]
    /// retrying the same closure against the same SQL will fail the same way.
    #[error(
        "raw SQL statement {statement} is not allowed inside an atomic() transaction; \
         use a transaction-local form (`SET LOCAL`, `SET CONSTRAINTS`, `SET TRANSACTION`) \
         or run the session-scoped statement on a pool-backed context"
    )]
    #[non_exhaustive]
    SessionStatementDisallowedInTransaction {
        /// Canonical top-level statement keyword that triggered the refusal.
        statement: &'static str,
    },

    /// A transaction-backed raw SQL call attempted to issue transaction-control
    /// SQL through the raw escape hatch instead of using Djogi's transaction
    /// lifecycle methods.
    /// This variant is used by the raw SQL bypass harness (#306) to reject
    /// transaction-control statements such as `BEGIN`, `START TRANSACTION`,
    /// `COMMIT`, `ROLLBACK`, `END`, `ABORT`, `SAVEPOINT`, `RELEASE [SAVEPOINT]`,
    /// and `ROLLBACK [WORK|TRANSACTION] TO [SAVEPOINT]` when the context is
    /// already inside an `atomic()` transaction. Those statements bypass
    /// framework bookkeeping: raw COMMIT skips `on_commit` callback drain,
    /// raw ROLLBACK skips rollback cleanup and callback discard, and raw
    /// savepoint control desynchronizes `savepoint_depth`.
    /// `statement` is the canonical top-level transaction-control keyword
    /// (`"BEGIN"`, `"COMMIT"`, etc.) that triggered the refusal. The fix is
    /// structural: use Djogi's `atomic()` / `commit()` / `rollback()` API, or
    /// run the transaction-control SQL on a pool-backed context outside any
    /// `atomic()` scope.
    /// Classified as **terminal** by [`DjogiError::is_transient`]
    /// retrying the same closure against the same SQL will fail the same way.
    #[error(
        "raw transaction-control statement {statement} is not allowed on a \
         transaction-backed DjogiContext; use djogi's transaction API so COMMIT \
         drains on_commit callbacks, ROLLBACK clears framework state, and savepoint \
         depth stays synchronized"
    )]
    #[non_exhaustive]
    RawTransactionControlDisallowedInTransaction {
        /// Canonical top-level transaction-control statement that triggered refusal.
        statement: &'static str,
    },

    /// An aggregate's DISTINCT modifier combination is not supported by
    /// Postgres syntax or by Djogi's current IR.
    /// # When this surfaces
    /// Raised by the fetch-time legality check in
    /// [`crate::expr::sql::check_aggregate_legality`] for three value-aggregate
    /// cases that cannot be represented by the kind-state split alone:
    /// - `COUNT(DISTINCT *)` — `COUNT(DISTINCT *)` is not valid SQL.
    ///   Use `COUNT(DISTINCT col)` via [`crate::query::field::FieldRef::count`]
    ///   instead.
    /// - `STRING_AGG(DISTINCT col, sep)` without per-aggregate `ORDER BY`
    ///   Postgres requires an explicit ordering when DISTINCT is combined
    ///   with `STRING_AGG`. Chain `.order_by(...)` on the aggregate to make
    ///   the shape well-formed.
    /// - `COUNT(*) ORDER BY ...` — the `COUNT(*)` emitter has no argument
    ///   slot to attach per-aggregate ordering to, so accepting the modifier
    ///   would silently drop it.
    ///   `op` is the aggregate function keyword (e.g. `"COUNT(*)"`,
    ///   `"STRING_AGG"`). `reason` is a human-readable description of why
    ///   the combination is rejected.
    #[error("unsupported aggregate: {op} — {reason}")]
    #[non_exhaustive]
    UnsupportedAggregate {
        /// The aggregate function keyword or name, e.g. `"COUNT(*)"` or
        /// `"STRING_AGG"`.
        op: &'static str,
        /// Human-readable explanation of why this combination is rejected.
        reason: &'static str,
    },

    /// The emitted SELECT list contains two columns with the same alias;
    /// the decoder would read the wrong value for one of the columns.
    /// This is a Djogi internal bug — a future API extension likely introduced
    /// a path that collides with a group-key column name or another aggregate
    /// alias. The check runs before any SQL is sent to Postgres so the
    /// collision is caught immediately rather than silently returning wrong
    /// data.
    #[error("alias collision in SELECT list: {alias}")]
    AliasCollision {
        /// The alias string that appears more than once in the SELECT list.
        alias: String,
    },

    /// A pool checkout exceeded its configured wait / create / recycle
    /// timeout. Pairs with
    /// [`DjogiPoolBuilder::timeout`](crate::pg::pool::DjogiPoolBuilder::timeout)
    /// so callers can branch on saturation explicitly without inspecting
    /// the underlying `deadpool_postgres::PoolError`.
    /// `phase` distinguishes the deadpool timeout type:
    /// - `"wait"` — the pool is at `max_size` and no slot freed within the
    ///   configured wait window. Tune `max_size` upward or stop holding
    ///   connections across awaits unrelated to the database.
    /// - `"create"` — `Manager::create` (opening a fresh socket) timed out.
    ///   Network or DB-side problem, not pool sizing.
    /// - `"recycle"` — recycling an existing object on the checkout path
    ///   timed out. Same root cause as `"create"` for `Verified`/`Clean`
    ///   recycling methods that issue queries.
    ///   All three are saturation / slow-recovery signals: the right
    ///   response is to back off and retry, not to fail the operation
    ///   permanently. [`DjogiError::is_transient`] returns `true` for
    ///   `PoolTimeout` so generic retry helpers that branch on
    ///   `is_transient` (or its inverse `is_terminal`) treat pool
    ///   timeouts as retryable rather than dead-lettering them as
    ///   permanent business failures.
    ///   Note that djogi exposes two retry helpers with different
    ///   policy: [`crate::transaction::retry_on_conflict`] retries
    ///   immediately, while
    ///   [`crate::transaction::retry_on_conflict_with_backoff`]
    ///   sleeps between transient failures using
    ///   [`crate::transaction::TransactionRetryBackoff`]. Pool
    ///   saturation usually belongs on the backoff path, not the
    ///   immediate-retry path. Callers that need a bespoke policy can
    ///   still match on `PoolTimeout` explicitly, and the backoff policy
    ///   can include/exclude `PoolTimeout` retries via
    ///   `with_retryable_error_classes(...)`.
    #[error("pool timeout ({phase})")]
    #[non_exhaustive]
    PoolTimeout {
        /// Which deadpool timeout fired — `"wait"`, `"create"`, or
        /// `"recycle"`. A `&'static str` because the set of phases is
        /// closed and tracking the exact variant lets callers match on it
        /// in tracing without depending on deadpool's enum.
        phase: &'static str,
    },

    /// `DjogiContext::set_role` was invoked on a pool-backed context
    /// rather than inside an `atomic` transaction.
    /// introduces this variant for the security-overlay row-level
    /// security helper: `SET LOCAL ROLE` is bound to the surrounding
    /// transaction and reverts at COMMIT/ROLLBACK, so calling it
    /// outside a transaction would either fail outright or — worse
    /// leak the role onto the pooled connection where the next
    /// checkout-victim would inherit it. Surfacing this as a
    /// dedicated variant lets callers branch on the misuse without
    /// inspecting the underlying SQLSTATE.
    /// Classified as **terminal** by [`DjogiError::is_transient`]
    /// retrying the same closure cannot turn a pool-backed context
    /// into a transactional one.
    #[error(
        "set_role can only be called inside an atomic() transaction; \
         pool-backed contexts have no transaction scope to bind SET LOCAL ROLE to"
    )]
    SetRoleOutsideTransaction,

    /// `DjogiContext::set_role` was invoked with a role name that
    /// fails the byte-level Postgres identifier check.
    /// introduces this variant so role-name validation surfaces
    /// before any SQL is sent — the framework refuses to interpolate
    /// an untrusted string into `SET LOCAL ROLE` even when the
    /// caller has already quoted it.
    /// The accepted grammar is the standard Postgres unquoted
    /// identifier shape: an ASCII letter or underscore followed by
    /// ASCII alphanumerics or underscores, up to 63 bytes. Embedded
    /// quotes, control characters, and non-ASCII bytes are all
    /// rejected. The variant carries the offending name so log
    /// lines and error reports can identify what was rejected.
    /// Classified as **terminal** by [`DjogiError::is_transient`]
    /// a malformed role name is a programming error, not a
    /// race-condition.
    #[error(
        "invalid Postgres role name {0:?}: must match Postgres identifier grammar \
         (ASCII letter or underscore followed by ASCII alphanumerics or underscores, \
         up to 63 bytes; no embedded quotes or control characters)"
    )]
    InvalidRoleName(String),

    /// A portable predicate could not be lowered to SQL.
    /// installs the direct-`Q<T>` SQL walker; portable predicate leaves
    /// dispatch through [`crate::model::Model::__djogi_emit_field_predicate`]
    /// (overridden by PR2d's macro on every PK-backed `#[model]`-emitted
    /// impl). Failure modes — unknown model, unknown field, unknown
    /// `LookupOp` for a known field, payload-shape mismatch, future
    /// Sassi predicate variant — surface here as a typed
    /// [`crate::query::PortablePredicateError`] before the SQL ever
    /// touches the database.
    /// Classified as **terminal** by [`DjogiError::is_transient`] — a
    /// portable-predicate lowering failure is a framework / model
    /// invariant violation, not a transient database condition.
    /// Retrying the same closure cannot turn an unknown field into a
    /// known one.
    #[error("predicate cannot be lowered to SQL: {0}")]
    Predicate(#[from] crate::query::PortablePredicateError),

    /// A [`SetOpQuerySet`](crate::query::SetOpQuerySet) arm carried
    /// state that cannot ride through a Postgres set-operation
    /// subquery: registered prefetch paths, registered select_related
    /// paths, a row-level lock, or a cache binding. Cluster
    /// 4B (issue #101) introduces this variant alongside the typed
    /// set-op surface (`.union(...)` / `.union_all(...)` /
    /// `.intersect(...)` / `.except(...)`).
    /// # Why this is a typed error, not a silent drop
    /// Quietly stripping `select_related` / `prefetch` registrations
    /// when an arm enters a set op would silently change the row
    /// shape the caller expected: `select_related` extends the
    /// projection, prefetch fans out follow-up queries on the
    /// returned rows. Both would either change the column count
    /// (breaking the set-op type compatibility rule) or silently drop
    /// data the caller asked for. Returning a typed error at the
    /// terminal — before any SQL hits the database — keeps the
    /// failure mode actionable. Locks (`FOR UPDATE`) inside a set-op
    /// subquery are rejected by Postgres at parse time anyway; we
    /// surface a higher-fidelity error before the round trip.
    /// `side` identifies which arm tripped the check (`"left"` or
    /// `"right"`); `reason` is a short human-readable explanation
    /// that names the offending registration.
    /// Classified as **terminal** by [`DjogiError::is_transient`]
    /// the caller built an incompatible set-op shape; retrying the
    /// same call cannot turn a `.cache(...)`-bound arm into a
    /// cache-free one.
    #[error(
        "set-op arm `{side}` on `{table}` is incompatible with set-operation subquery: {reason}"
    )]
    #[non_exhaustive]
    SetOpArmInvalid {
        table: &'static str,
        side: &'static str,
        reason: &'static str,
    },

    /// A [`SetOpQuerySet`](crate::query::SetOpQuerySet)'s outer
    /// `ORDER BY` carried an expression-form ordering term that
    /// Postgres rejects on set-operation outer ordering.
    /// (issue #101) introduces this variant alongside the
    /// typed set-op surface.
    /// # Why this is a typed error, not a silent pass-through
    /// Postgres set-operation `ORDER BY` only accepts output column
    /// names (or column position numbers) — arbitrary expressions are
    /// rejected at parse time. Today the only way to produce a
    /// non-column outer ordering is the spatial
    /// `order_by_distance(...)` helper, which emits a `ST_Distance(...)`
    /// expression. Letting that ride through to Postgres would surface
    /// a low-level parser error (`syntax error at or near "("`,
    /// `ORDER BY position out of range`, or similar) that does not name
    /// the offending operation. Djogi catches the case at SQL-build
    /// time and surfaces a higher-fidelity error before the round
    /// trip, naming the table and explaining the constraint.
    /// `table` identifies the model whose set-op carries the
    /// incompatible ordering; `reason` is a short human-readable
    /// explanation that names the kind of ordering rejected and the
    /// recommended workaround.
    /// Classified as **terminal** by [`DjogiError::is_transient`]
    /// the caller built an incompatible set-op shape; retrying the
    /// same call cannot turn an expression-form ordering into a
    /// column-form one. The fix is at the call site.
    #[error("set-op outer ORDER BY on `{table}` is incompatible with set-operation: {reason}")]
    #[non_exhaustive]
    SetOpOuterOrderingInvalid {
        table: &'static str,
        reason: &'static str,
    },

    /// `djogi::transaction::atomic_with(level, &mut tx_ctx, ...)` was
    /// invoked on a transaction-backed [`crate::DjogiContext`] — i.e.
    /// inside an already-open `atomic` scope.
    /// (issue #168) introduces this variant alongside the typed
    /// [`crate::transaction::IsolationLevel`] surface.
    /// Postgres pins the isolation level for the entire transaction at
    /// the outer `BEGIN`; `SAVEPOINT` does not open a sub-transaction
    /// with its own isolation knob, and `SET TRANSACTION ISOLATION
    /// LEVEL` issued after the first non-control statement is rejected
    /// with SQLSTATE `25001` (active SQL transaction). Surfacing this
    /// as a typed variant lets callers branch on the misuse before
    /// the SQL flies; the alternative would be a deferred SQLSTATE
    /// surprise that names neither the outer BEGIN nor the requested
    /// level.
    /// The variant carries the [`crate::transaction::IsolationLevel`]
    /// the caller requested so log lines and error reports identify
    /// what was rejected. Use [`crate::transaction::atomic`] for
    /// nested scopes — the savepoint inherits the outermost
    /// transaction's isolation level.
    /// Classified as **terminal** by [`DjogiError::is_transient`] — a
    /// nested-scope isolation request is a programming error, not a
    /// race condition. Retrying the same closure cannot turn a
    /// savepoint into a fresh outermost transaction.
    #[error(
        "atomic_with(level={requested}) called inside an open atomic() scope; \
         Postgres pins the isolation level at the outer BEGIN, savepoints \
         cannot change it — use atomic() for nested scopes, or move the \
         atomic_with call outside the enclosing transaction"
    )]
    IsolationLevelOnNestedScope {
        /// Isolation level the caller requested. `&'static`-like
        /// the enum is `Copy` so logging callers can read it without
        /// borrowing the variant.
        requested: crate::transaction::IsolationLevel,
    },

    /// `DjogiContext::defer_constraints` / `set_constraints_immediate`
    /// was invoked on a pool-backed context rather than inside an
    /// `atomic` transaction. issue #169)
    /// introduces this variant alongside the typed
    /// [`crate::transaction::DeferScope`] surface.
    /// `SET CONSTRAINTS` is transaction-scoped in Postgres — outside a
    /// transaction it would either fail outright or, on the
    /// implicit per-statement transaction surrounding a single
    /// statement, evaporate before any subsequent statement could
    /// observe the deferred state. Both outcomes are programming
    /// errors, so the framework refuses to issue the SQL.
    /// Classified as **terminal** by [`DjogiError::is_transient`]
    /// retrying cannot turn a pool-backed context into a
    /// transactional one. Wrap the call in
    /// [`crate::transaction::atomic`] to get a transaction scope.
    #[error(
        "defer_constraints / set_constraints_immediate can only be called \
         inside an atomic() transaction; pool-backed contexts have no \
         transaction scope for SET CONSTRAINTS to bind to"
    )]
    ConstraintModeOutsideTransaction,

    /// `DjogiContext::defer_constraints` /
    /// `set_constraints_immediate` was called with a
    /// [`crate::transaction::DeferScope::Named`] payload that
    /// referenced an unknown constraint. issue
    /// #169) introduces this variant.
    /// "Unknown" means the constraint name was not found on any
    /// `#[derive(Model)]`-emitted [`crate::DeferrabilitySpec`]
    /// inventory entry. The lookup uses the conventional
    /// `<table>_<column>_fkey` shape (`{table}_{column}_fkey`,
    /// truncated to Postgres' 63-byte identifier limit when
    /// necessary) for foreign-key constraints declared in
    /// adopter `#[model]` structs.
    /// Surfacing the typo as a typed error before the SQL flies is
    /// the value-add over `ctx.raw_execute("SET CONSTRAINTS \"typo\"
    /// DEFERRED")`: Postgres would raise `42704
    /// (undefined_object)` for an unknown constraint, but only
    /// after a round trip and without naming the descriptor it
    /// should have come from.
    /// The variant carries the offending name so log lines identify
    /// what was rejected. Classified as **terminal** by
    /// [`DjogiError::is_transient`] — retrying cannot turn an
    /// unknown name into a known one.
    #[error(
        "unknown constraint name {0:?} — no `#[derive(Model)]`-declared FK \
         registers under that name. Expected names follow the convention \
         `<table>_<column>_fkey` (truncated to 63 bytes for long names)"
    )]
    UnknownConstraintName(String),

    /// `DjogiContext::defer_constraints` was called with a
    /// [`crate::transaction::DeferScope::Named`] payload that
    /// referenced a constraint declared as
    /// non-deferrable (`#[field(deferrable = false)]`, the default).
    /// Issue #169) introduces this variant.
    /// Postgres rejects `SET CONSTRAINTS <name> DEFERRED` on a
    /// non-deferrable constraint with SQLSTATE `0A000`
    /// (feature_not_supported). The framework checks the descriptor's
    /// [`crate::DeferrabilitySpec`] inventory and surfaces a typed
    /// error before the SQL flies — same value-add as
    /// [`Self::UnknownConstraintName`].
    /// The fix is at the model declaration: declare the FK as
    /// `#[field(deferrable = true)]` (and optionally
    /// `initially_deferred = true` for `DEFERRABLE INITIALLY
    /// DEFERRED`). The constraint must be deferrable to participate
    /// in `SET CONSTRAINTS` at runtime.
    /// Classified as **terminal** by [`DjogiError::is_transient`] — a
    /// non-deferrable constraint cannot be deferred at runtime
    /// regardless of how many retries.
    #[error(
        "constraint {0:?} is not declared deferrable; SET CONSTRAINTS only \
         applies to constraints declared `DEFERRABLE`. Declare the FK with \
         `#[field(deferrable = true)]` (and optionally `initially_deferred = \
         true`) at the model declaration"
    )]
    ConstraintNotDeferrable(String),

    /// `DjogiContext::defer_constraints` /
    /// `set_constraints_immediate` was called with
    /// [`crate::transaction::DeferScope::Named`] carrying an empty
    /// slice. issue #169)
    /// follow-up fix.
    /// `SET CONSTRAINTS <name list> DEFERRED|IMMEDIATE` requires at
    /// least one name; Postgres rejects the bare-comma grammar with
    /// SQLSTATE `42601` (syntax error). Composing the SQL from an
    /// empty slice would produce `SET CONSTRAINTS DEFERRED` — an
    /// extra space + missing list. Reject before SQL composition so
    /// the caller gets a typed error naming the misuse rather than a
    /// deferred Postgres parse error.
    /// The canonical fix is one of:
    /// - drop the `Named` wrapper and use [`DeferScope::All`](crate::transaction::DeferScope::All)
    ///   if the intent is "every deferrable constraint";
    /// - skip the call entirely when the names slice is empty;
    /// - pass at least one valid constraint name.
    ///   Classified as **terminal** by [`DjogiError::is_transient`]
    ///   retrying with the same empty slice cannot succeed.
    #[error(
        "DeferScope::Named requires at least one constraint name; \
         empty slices produce malformed `SET CONSTRAINTS` SQL. Use \
         `DeferScope::All` to target every deferrable constraint, or \
         skip the call when the list is empty"
    )]
    EmptyDeferConstraintsScope,

    /// Runtime inventory walk for
    /// [`DjogiContext::defer_constraints`] /
    /// [`DjogiContext::set_constraints_immediate`] observed two
    /// [`crate::DeferrabilitySpec`] entries sharing the same
    /// `(model_type_name, field_name)` key but disagreeing on
    /// `(deferrable, initially_deferred)`. issue
    /// #169) — .
    /// `inventory::iter` order is not deterministic across builds.
    /// A silent last-writer-wins on a disagreeing duplicate would
    /// make the runtime validator non-deterministic — one build
    /// would accept a `SET CONSTRAINTS <name> DEFERRED` request, the
    /// next would reject it as `ConstraintNotDeferrable`. Mirror the
    /// projection-time [`ConflictingDeferrabilitySpec`] gate in
    /// `migrate::projection` so the framework fails closed at both
    /// schema-build time and transaction-control time.
    /// Idempotent duplicates (same key, identical values) are
    /// accepted — they can arise from `inventory::submit!` chains
    /// across crates re-exporting the same model and carry no
    /// semantic disagreement.
    /// Classified as **terminal** by [`DjogiError::is_transient`]
    /// the conflict is a build-time inventory misconfiguration, not
    /// a race condition. Fix is at the model declaration.
    /// [`ConflictingDeferrabilitySpec`]: crate::migrate::projection::ProjectionError::ConflictingDeferrabilitySpec
    #[error(
        "conflicting DeferrabilitySpec inventory entries for \
         {model_type_name}.{field_name}: \
         first = {first:?}, second = {second:?}. Two `#[derive(Model)]` \
         emissions disagree on `(deferrable, initially_deferred)`; \
         resolve the duplicate model definition at the source"
    )]
    ConflictingDeferrabilitySpec {
        /// Rust type name carrying the FK field.
        model_type_name: String,
        /// Field name (Postgres column name).
        field_name: String,
        /// `(deferrable, initially_deferred)` from the first spec
        /// the validator observed.
        first: (bool, bool),
        /// `(deferrable, initially_deferred)` from the second spec
        /// the validator observed.
        second: (bool, bool),
    },

    /// Runtime inventory walk for
    /// [`DjogiContext::defer_constraints`] /
    /// [`DjogiContext::set_constraints_immediate`] observed a
    /// [`crate::DeferrabilitySpec`] whose `model_type_name` has no
    /// matching [`crate::ModelDescriptor`] entry. Cluster
    /// 4 (issue #169) — .
    /// The descriptor is the source of truth for `type_name →
    /// table_name`. A `DeferrabilitySpec` without a matching
    /// `ModelDescriptor` means the FK is not really registered, but
    /// would be silently skipped by the prior implementation. That
    /// silent skip is the bug: a valid-looking constraint name
    /// `<expected_table>_<field>_fkey` would then surface as
    /// [`UnknownConstraintName`](Self::UnknownConstraintName)
    /// instead of the actual root cause (the missing descriptor).
    /// `#[derive(Model)]` emits the descriptor + the deferrability
    /// spec side by side, so an orphan spec only fires under
    /// pathological partial-emission conditions — typically a
    /// hand-written `inventory::submit!` outside the macro.
    /// Classified as **terminal** by [`DjogiError::is_transient`]
    /// the cause is a build-time inventory misconfiguration.
    #[error(
        "orphan DeferrabilitySpec for {model_type_name}.{field_name}: \
         no matching ModelDescriptor is registered in the inventory. \
         `#[derive(Model)]` emits both side by side; the orphan \
         indicates a hand-written `inventory::submit!` outside the \
         macro or a partial-emit bug"
    )]
    OrphanDeferrabilitySpec {
        /// Rust type name carrying the orphan FK field.
        model_type_name: String,
        /// Field name (Postgres column name).
        field_name: String,
    },

    /// Runtime inventory walk for
    /// [`DjogiContext::defer_constraints`] /
    /// [`DjogiContext::set_constraints_immediate`] observed two
    /// distinct `(model_type_name, field_name)` pairs whose
    /// conventional FK constraint names collide. Cluster
    /// 4 (issue #169) — .
    /// The constraint-name convention is
    /// `<table>_<column>_fkey` (truncated to Postgres' 63-byte
    /// identifier limit). Truncation can produce collisions for
    /// long table or column names — and a collision means the
    /// runtime validator has no way to know which FK the adopter
    /// meant. Fail closed.
    /// The fix at the model declaration is to shorten the offending
    /// table or column name, or to declare an explicit constraint
    /// name once that surface lands (out of scope for #169).
    /// Classified as **terminal** by [`DjogiError::is_transient`]
    /// the conflict is a build-time naming collision.
    #[error(
        "FK constraint name {constraint_name:?} collides across two \
         distinct fields: ({first_model}.{first_field}) and \
         ({second_model}.{second_field}). Postgres' 63-byte identifier \
         limit truncates long `<table>_<column>_fkey` strings; shorten \
         the offending table or column name to disambiguate"
    )]
    DuplicateConstraintName {
        /// The constraint name that two distinct fields produce.
        constraint_name: String,
        /// First model the validator observed under this name.
        first_model: String,
        /// First field the validator observed under this name.
        first_field: String,
        /// Second model the validator observed under this name.
        second_model: String,
        /// Second field the validator observed under this name.
        second_field: String,
    },

    /// `QuerySet::merge_into` was invoked on a transaction-backed context. (issue #173)
    /// introduces this variant alongside the typed concurrent-reads
    /// helper.
    /// A transaction owns one Postgres connection; cloning the
    /// context for concurrent reads would either hand out aliasing
    /// access to the same connection (Postgres protocol violation)
    /// or silently break the transaction boundary. Both are
    /// programming errors, so the framework refuses.
    /// The correct shape for concurrent reads is on a pool-backed
    /// context: each clone gets a fresh pool checkout, the two
    /// contexts operate on independent connections, and
    /// `tokio::try_join!` over typed reads composes without an
    /// `E0499` mutable-borrow conflict.
    /// Classified as **terminal** by [`DjogiError::is_transient`]
    /// retrying cannot turn a transaction-backed context into a
    /// pool-backed one. Move the concurrent-reads block outside the
    /// surrounding `atomic()`.
    #[error(
        "clone_for_concurrent_reads requires a pool-backed DjogiContext; \
         transaction-backed contexts own a single connection that cannot \
         be aliased across concurrent reads. Move the concurrent-reads \
         block outside the surrounding atomic() scope, or fetch \
         sequentially"
    )]
    ConcurrentReadsRequirePoolContext,

    /// `QuerySet::merge_into` observed a source queryset with state that
    /// cannot be safely represented in a `MERGE` statement: `prefetch`,
    /// `select_related`, `cache`, `lock`, or `distinct`.
    /// Issue #178.
    /// Classified as **terminal** by [`DjogiError::is_transient`].
    #[error("merge source queryset on `{table}` is invalid: {reason}")]
    #[non_exhaustive]
    MergeSourceInvalid {
        table: &'static str,
        reason: &'static str,
    },

    /// `QuerySet::merge_into` observed an invalid branch configuration:
    /// unreachable branches (same-kind unconditional branch follows another),
    /// duplicate target columns in an update or insert action, or manual
    /// `updated_at` assignment in an update action. Issue #178.
    /// Classified as **terminal** by [`DjogiError::is_transient`].
    #[error("merge branch on `{table}` is invalid: {reason}")]
    #[non_exhaustive]
    MergeBranchInvalid { table: &'static str, reason: String },

    /// `QuerySet::merge_into` was invoked without any `ON` conditions or
    /// without any `WHEN` branches. Issue #178.
    /// Classified as **terminal** by [`DjogiError::is_transient`].
    #[error("merge statement on `{table}` is invalid: {reason}")]
    #[non_exhaustive]
    MergeNoBranches { table: &'static str, reason: String },

    /// One or more presentation codecs failed startup validation.
    /// Returned by
    /// [`validate_startup_inventory`](crate::presentation::validate_startup_inventory)
    /// when any [`PresentationCodecUsage`](crate::presentation::inventory::PresentationCodecUsage)
    /// entry's `validate_startup` hook returns an error.
    /// This variant is the conversion target for pool-construction callers
    /// (`DjogiPool::connect`, `DjogiPool::from_database_config`,
    /// `DjogiPoolBuilder::build`) that call `validate_startup_inventory`
    /// before accepting traffic. GH #227 wires those callers.
    /// The inner `Vec` carries one
    /// [`PresentationStartupError`](crate::presentation::PresentationStartupError)
    /// per failing codec usage. Each entry names the `(model, field, scope,
    /// codec)` quadruple and the underlying error so operators can identify
    /// every misconfigured codec in one pass rather than discovering failures
    /// one at a time.
    /// Classified as **terminal** by the framework — a codec with a missing
    /// or invalid key cannot serve traffic until the key is provided. The
    /// fix is an environment-variable or configuration change, not a retry.
    /// `Display` includes the total count plus a concise summary of each
    /// failing usage so operator logs can point directly at the actionable
    /// `(model, field, scope, codec)` entry without requiring `Debug`.
    #[error(
        "presentation codec startup validation failed ({} error(s)): {}",
        .0.len(),
        crate::error::presentation_startup_error_summary(&.0)
    )]
    PresentationStartup(Vec<crate::presentation::PresentationStartupError>),

    /// Connected PostgreSQL server version is below Djogi's minimum supported
    /// version. Raised by [`check_postgres_version`](crate::pg::preflight::check_postgres_version)
    /// preflight when the detected major version is less than 18.
    /// `detected_major` and `detected_minor` are the actual server version components.
    /// `minimum_major` is Djogi's minimum supported major version (always 18).
    #[error(
        "PostgreSQL {detected_major}.{detected_minor} is below the minimum supported \
         version {minimum_major}. Upgrade to PostgreSQL {minimum_major} or later"
    )]
    #[non_exhaustive]
    UnsupportedPostgresVersion {
        detected_major: u32,
        detected_minor: u32,
        minimum_major: u32,
    },
}

/// Bridge: convert `tokio_postgres::Error` into `DjogiError`.
impl From<tokio_postgres::Error> for DjogiError {
    fn from(e: tokio_postgres::Error) -> Self {
        map_pg_err(e)
    }
}

/// `Infallible` → `DjogiError` coercion.
/// `Infallible` has no inhabitants, so `match never {}` is exhaustive.
/// The impl exists so macro-emitted chains that invoke
/// `<Visage as TryFrom<&Model>>::try_from(row)?` propagate through `?`
/// uniformly regardless of whether the visage is scalar-only (the
/// stdlib blanket returns `Infallible`) or relation-nesting (returns
/// `VisageError`). Without this impl the scalar-only path fails
/// compilation with "`?` couldn't convert the error to `DjogiError`".
impl From<std::convert::Infallible> for DjogiError {
    fn from(never: std::convert::Infallible) -> Self {
        match never {}
    }
}

impl DjogiError {
    /// Construct a `NotFound` error with a table-name context.
    /// This is the public escape hatch for `#[non_exhaustive]` on the
    /// `NotFound` variant: struct-expression construction is blocked outside
    /// this crate, so `#[model]`-expanded CRUD methods (which run in user
    /// crates) call this constructor instead. Keep the signature stable
    /// any future additional context fields must gain their own constructor
    /// or builder rather than changing this one.
    pub fn not_found(table: &'static str) -> Self {
        DjogiError::NotFound { table }
    }

    /// Construct a `MultipleObjects` error with a table name and the number
    /// of rows actually observed.
    /// Mirror of `not_found` — exists so that cross-crate callers (macro
    /// output, future filter-layer builders) can produce this variant
    /// without running into `#[non_exhaustive]`.
    pub fn multiple_objects(table: &'static str, count_seen: usize) -> Self {
        DjogiError::MultipleObjects { table, count_seen }
    }

    /// Construct a `RelationUnloaded` error naming the model and relation
    /// field that the caller asked to resolve without loading.
    /// Exists for the same reason as `not_found` / `multiple_objects`: the
    /// `#[non_exhaustive]` attribute on the variant blocks struct-expression
    /// construction outside this crate, so macro-expanded code and +
    /// relation wrappers go through this constructor instead.
    pub fn relation_unloaded(model: &'static str, field: &'static str) -> Self {
        DjogiError::RelationUnloaded { model, field }
    }

    /// Construct a `MissingIdempotencyKey` error naming the model
    /// whose `#[model(idempotency_key = "...")]` attribute was not
    /// declared.
    /// Mirror of `not_found` / `multiple_objects` — exists so that
    /// cross-crate callers (macro output in user crates) can produce
    /// this variant despite `#[non_exhaustive]` blocking struct-
    /// expression construction outside this crate.
    pub fn missing_idempotency_key(model: &'static str) -> Self {
        DjogiError::MissingIdempotencyKey { model }
    }

    /// Construct a `GoneAggregate` error.
    /// Mirror of the other constructors — exists so that cross-crate
    /// callers can produce this `#[non_exhaustive]` variant. `id` is
    /// typically produced via `format!("{}", pk)` so the error is
    /// independent of the originating model's `Pk` type.
    pub fn gone_aggregate(model: &'static str, id: String, reason: &'static str) -> Self {
        DjogiError::GoneAggregate { model, id, reason }
    }

    /// Construct an `UnsupportedPostgresVersion` error with the detected
    /// server version and the framework's minimum supported major version.
    /// Mirror of the other constructors — exists so that cross-crate callers
    /// (the `pg::preflight` module, CLI entry points) can produce this variant
    /// despite `#[non_exhaustive]` blocking struct-expression construction
    /// outside this crate.
    pub fn unsupported_postgres_version(
        detected_major: u32,
        detected_minor: u32,
        minimum_major: u32,
    ) -> Self {
        DjogiError::UnsupportedPostgresVersion {
            detected_major,
            detected_minor,
            minimum_major,
        }
    }

    /// Classify this error as **transient** (retrying the closure
    /// may succeed) or **terminal** (retrying will not help).
    /// `retry_on_conflict` uses this predicate to decide whether to
    /// re-run its closure. The contract:
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
    /// | [`TransactionPoisoned`](Self::TransactionPoisoned) | terminal |
    /// | [`SessionStatementDisallowedInTransaction`](Self::SessionStatementDisallowedInTransaction) | terminal |
    /// | [`RawTransactionControlDisallowedInTransaction`](Self::RawTransactionControlDisallowedInTransaction) | terminal |
    /// | [`PoolTimeout`](Self::PoolTimeout) | transient |
    /// | [`SetRoleOutsideTransaction`](Self::SetRoleOutsideTransaction) | terminal |
    /// | [`InvalidRoleName`](Self::InvalidRoleName) | terminal |
    /// | [`IsolationLevelOnNestedScope`](Self::IsolationLevelOnNestedScope) | terminal |
    /// | [`ConstraintModeOutsideTransaction`](Self::ConstraintModeOutsideTransaction) | terminal |
    /// | [`UnknownConstraintName`](Self::UnknownConstraintName) | terminal |
    /// | [`ConstraintNotDeferrable`](Self::ConstraintNotDeferrable) | terminal |
    /// | [`EmptyDeferConstraintsScope`](Self::EmptyDeferConstraintsScope) | terminal |
    /// | [`ConflictingDeferrabilitySpec`](Self::ConflictingDeferrabilitySpec) | terminal |
    /// | [`OrphanDeferrabilitySpec`](Self::OrphanDeferrabilitySpec) | terminal |
    /// | [`DuplicateConstraintName`](Self::DuplicateConstraintName) | terminal |
    /// | [`ConcurrentReadsRequirePoolContext`](Self::ConcurrentReadsRequirePoolContext) | terminal |
    /// | [`UnsupportedPostgresVersion`](Self::UnsupportedPostgresVersion) | terminal |
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
            // Pool saturation / slow connection creation / slow recycle
            // are all retry-with-backoff conditions, not permanent
            // failures. Generic retry helpers branching on
            // `is_transient` should treat these as transient; callers
            // wanting bespoke `PoolTimeout`-vs-`LockConflict` policy
            // should match on the variant explicitly.
            DjogiError::PoolTimeout { .. } => true,
            _ => false,
        }
    }

    /// Inverse of [`is_transient`](Self::is_transient) — returns
    /// `true` when retrying will not help.
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
/// Matches three SQLSTATEs:
/// - `40001` (`serialization_failure`) — the classic MVCC serialization
///   error on `SERIALIZABLE`/`REPEATABLE READ` isolation.
/// - `40P01` (`deadlock_detected`) — Postgres detected a circular wait
///   and aborted one of the participants.
/// - `55P03` (`lock_not_available`) — a `NOWAIT` lock request could not
///   acquire its lock immediately.
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
        assert!(
            DjogiError::Visage(crate::visage::VisageError::UnresolvedRelation {
                model: "M",
                field: "f",
                scope: "public"
            })
            .is_terminal(),
            "Visage conversion failures must be terminal unless explicitly reclassified"
        );
        assert!(
            DjogiError::PresentationStartup(vec![]).is_terminal(),
            "PresentationStartup must be terminal — missing startup prerequisites need operator action"
        );
        let presentation_startup_msg = DjogiError::PresentationStartup(vec![
            crate::presentation::PresentationStartupError::MissingEnvVar {
                name: "DJOGI_PRESENTATION_HMAC_KEY",
            },
            crate::presentation::PresentationStartupError::Usage {
                model: "User",
                field: "email",
                scope: "public",
                codec_path: "djogi::presentation::builtins::HmacSha256HexString",
                source: Box::new(
                    crate::presentation::PresentationStartupError::MissingEnvVar {
                        name: "DJOGI_PRESENTATION_HMAC_KEY",
                    },
                ),
            },
        ])
        .to_string();
        assert!(
            presentation_startup_msg.contains("2 error(s)"),
            "PresentationStartup display must include the error count: {presentation_startup_msg}"
        );
        assert!(
            presentation_startup_msg.contains("missing env var `DJOGI_PRESENTATION_HMAC_KEY`"),
            "PresentationStartup display must include the actionable inner error: {presentation_startup_msg}"
        );
        assert!(
            presentation_startup_msg.contains("User.email scope `public` codec `djogi::presentation::builtins::HmacSha256HexString` failed"),
            "PresentationStartup display must include the usage context: {presentation_startup_msg}"
        );
        assert!(
            DjogiError::PoolTimeout { phase: "wait" }.is_transient(),
            "PoolTimeout must be transient — saturation is a retry-with-backoff condition"
        );
        assert!(
            !DjogiError::PoolTimeout { phase: "wait" }.is_terminal(),
            "PoolTimeout must NOT be terminal — generic retry helpers must not dead-letter it"
        );
        assert!(
            DjogiError::TransactionPoisoned {
                reason: "nested atomic future dropped before savepoint cleanup"
            }
            .is_terminal(),
            "TransactionPoisoned must be terminal — retry cannot clean the same context"
        );
        assert!(
            DjogiError::SetRoleOutsideTransaction.is_terminal(),
            "SetRoleOutsideTransaction must be terminal — retry cannot promote a pool-backed context"
        );
        assert!(
            DjogiError::InvalidRoleName("readonly".into()).is_terminal(),
            "InvalidRoleName must be terminal — a malformed role name is a programming error"
        );
        // #101) — set-op outer ORDER BY rejection.
        // An expression-form outer ordering cannot become a column-form
        // one by retrying; the fix is at the call site.
        assert!(
            DjogiError::SetOpOuterOrderingInvalid {
                table: "t",
                reason: "spatial ST_Distance(...) is an expression, not an output column"
            }
            .is_terminal(),
            "SetOpOuterOrderingInvalid must be terminal — retry cannot reshape the ordering"
        );
        assert!(
            DjogiError::MergeSourceInvalid {
                table: "t",
                reason: "prefetch is not supported"
            }
            .is_terminal(),
            "MergeSourceInvalid must be terminal"
        );
        assert!(
            DjogiError::MergeBranchInvalid {
                table: "t",
                reason: "duplicate column".into()
            }
            .is_terminal(),
            "MergeBranchInvalid must be terminal"
        );
        assert!(
            DjogiError::MergeNoBranches {
                table: "t",
                reason: "at least one branch required".into()
            }
            .is_terminal(),
            "MergeNoBranches must be terminal"
        );
        assert!(
            DjogiError::unsupported_postgres_version(17, 4, 18).is_terminal(),
            "UnsupportedPostgresVersion must be terminal — version mismatch cannot be resolved by retrying"
        );
        assert!(
            DjogiError::RawTransactionControlDisallowedInTransaction {
                statement: "COMMIT"
            }
            .is_terminal(),
            "RawTransactionControlDisallowedInTransaction must be terminal"
        );
    }

    /// 1 — `SetRoleOutsideTransaction` is a misuse signal,
    /// never retryable. Mirrors the `StreamOutsideTransaction`
    /// classification: the caller must restructure their code to wrap
    /// the call in `atomic()`, not back off and retry.
    #[test]
    fn set_role_outside_transaction_is_terminal() {
        let err = DjogiError::SetRoleOutsideTransaction;
        assert!(
            err.is_terminal(),
            "SetRoleOutsideTransaction must be terminal"
        );
        assert!(
            !err.is_transient(),
            "SetRoleOutsideTransaction must not be transient"
        );
    }

    /// 1 — `InvalidRoleName` is a validation error;
    /// retrying with the same string would fail again. The variant
    /// carries the offending name but the classification is fixed.
    #[test]
    fn invalid_role_name_is_terminal() {
        let err = DjogiError::InvalidRoleName("x".to_string());
        assert!(err.is_terminal(), "InvalidRoleName must be terminal");
        assert!(!err.is_transient(), "InvalidRoleName must not be transient");
    }

    /// 1 — the `Display` formatter uses `{0:?}` to debug-
    /// quote the offending role name. This protects log lines from
    /// confusion when the input contains embedded quotes, semicolons,
    /// or other shell/SQL-loaded characters: the rendered form makes
    /// the exact bytes unambiguous.
    #[test]
    fn invalid_role_name_display_includes_offending_value() {
        let err = DjogiError::InvalidRoleName("readonly\"; DROP TABLE".into());
        let msg = format!("{err}");
        assert!(
            msg.contains("readonly\\\"; DROP TABLE"),
            "expected debug-quoted role name in error message, got: {msg}"
        );
    }

    /// #168 — `IsolationLevelOnNestedScope` is a programming
    /// error. Postgres pins isolation at the outer BEGIN; retrying the
    /// same nested call cannot make Postgres reset isolation
    /// mid-transaction. The variant must classify as terminal so
    /// generic retry helpers do not pointlessly re-run the closure.
    #[test]
    fn isolation_level_on_nested_scope_is_terminal() {
        let err = DjogiError::IsolationLevelOnNestedScope {
            requested: crate::transaction::IsolationLevel::Serializable,
        };
        assert!(
            err.is_terminal(),
            "IsolationLevelOnNestedScope must be terminal"
        );
        assert!(
            !err.is_transient(),
            "IsolationLevelOnNestedScope must not be transient"
        );
    }

    /// #168 — `Display` for `IsolationLevelOnNestedScope`
    /// includes the requested isolation level so operators reading
    /// logs can identify what was rejected without consulting the
    /// stack trace.
    #[test]
    fn isolation_level_on_nested_scope_display_includes_level() {
        let err = DjogiError::IsolationLevelOnNestedScope {
            requested: crate::transaction::IsolationLevel::RepeatableRead,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("REPEATABLE READ"),
            "expected requested isolation level in message, got: {msg}"
        );
    }

    /// #281 — a nested atomic cancellation poisons the parent
    /// transaction. The only safe next step is rollback and retry from a fresh
    /// outer transaction, so generic retry classifiers must not treat the same
    /// context as reusable.
    #[test]
    fn transaction_poisoned_is_terminal() {
        let err = DjogiError::TransactionPoisoned {
            reason: "nested atomic future dropped before savepoint cleanup",
        };
        assert!(err.is_terminal(), "TransactionPoisoned must be terminal");
        assert!(
            !err.is_transient(),
            "TransactionPoisoned must not be transient"
        );
    }

    /// #282 — refusing a session-scoped raw statement inside an
    /// existing transaction is a caller-structure error, not a transient
    /// runtime failure. Retrying the same closure against the same SQL will
    /// fail the same way.
    #[test]
    fn session_statement_disallowed_in_transaction_is_terminal() {
        let err = DjogiError::SessionStatementDisallowedInTransaction { statement: "SET" };
        assert!(
            err.is_terminal(),
            "SessionStatementDisallowedInTransaction must be terminal"
        );
        assert!(
            !err.is_transient(),
            "SessionStatementDisallowedInTransaction must not be transient"
        );
    }

    /// Issue #306 — refusing a transaction-control raw statement inside an
    /// existing transaction is a caller-structure error. Raw COMMIT bypasses
    /// on_commit drain; raw ROLLBACK bypasses rollback cleanup and callback
    /// discard; raw savepoint control desynchronizes savepoint_depth.
    #[test]
    fn raw_transaction_control_disallowed_in_transaction_is_terminal() {
        let err = DjogiError::RawTransactionControlDisallowedInTransaction {
            statement: "COMMIT",
        };
        assert!(
            err.is_terminal(),
            "RawTransactionControlDisallowedInTransaction must be terminal"
        );
        assert!(
            !err.is_transient(),
            "RawTransactionControlDisallowedInTransaction must not be transient"
        );

        let msg = err.to_string();
        assert!(
            msg.contains("COMMIT"),
            "display must name the refused statement, got: {msg}"
        );
        assert!(
            msg.contains("transaction-backed"),
            "display must explain this is about transaction-backed contexts, got: {msg}"
        );
    }

    /// #169 — `ConstraintModeOutsideTransaction` mirrors the
    /// `SetRoleOutsideTransaction` classification: a caller invoking a
    /// transaction-scoped helper on a pool-backed context must
    /// restructure their code to wrap the call in `atomic()`, not back
    /// off and retry.
    #[test]
    fn constraint_mode_outside_transaction_is_terminal() {
        let err = DjogiError::ConstraintModeOutsideTransaction;
        assert!(
            err.is_terminal(),
            "ConstraintModeOutsideTransaction must be terminal"
        );
        assert!(
            !err.is_transient(),
            "ConstraintModeOutsideTransaction must not be transient"
        );
    }

    /// #169 — `UnknownConstraintName` is a validation
    /// error; retry with the same string would fail again. The
    /// variant carries the offending name verbatim so log scrapers
    /// can identify what was rejected.
    #[test]
    fn unknown_constraint_name_is_terminal() {
        let err = DjogiError::UnknownConstraintName("typo_fkey".into());
        assert!(err.is_terminal());
        assert!(!err.is_transient());
        let msg = format!("{err}");
        assert!(
            msg.contains("typo_fkey"),
            "expected offending name in message, got: {msg}"
        );
    }

    /// #169 — `ConstraintNotDeferrable` is terminal because
    /// a constraint declared non-deferrable cannot be deferred at
    /// runtime; the fix is at the model declaration. Verifies that
    /// the message names the offending constraint.
    #[test]
    fn constraint_not_deferrable_is_terminal() {
        let err = DjogiError::ConstraintNotDeferrable("posts_author_id_fkey".into());
        assert!(err.is_terminal());
        assert!(!err.is_transient());
        let msg = format!("{err}");
        assert!(
            msg.contains("posts_author_id_fkey"),
            "expected offending name in message, got: {msg}"
        );
        assert!(
            msg.contains("deferrable"),
            "expected remediation hint mentioning `deferrable`, got: {msg}"
        );
    }

    /// #173 — `ConcurrentReadsRequirePoolContext` is
    /// terminal because the fix is structural (move outside
    /// `atomic()`), not transient. Mirrors the
    /// `SetRoleOutsideTransaction` shape.
    #[test]
    fn concurrent_reads_require_pool_context_is_terminal() {
        let err = DjogiError::ConcurrentReadsRequirePoolContext;
        assert!(err.is_terminal());
        assert!(!err.is_transient());
    }

    /// #169 (
    /// `EmptyDeferConstraintsScope` is terminal because the empty
    /// slice is a programming error that retries cannot resolve.
    /// The message must mention `DeferScope::All` as the remediation
    /// hint for the common "I just meant everything" mistake.
    #[test]
    fn empty_defer_constraints_scope_is_terminal_and_names_alternative() {
        let err = DjogiError::EmptyDeferConstraintsScope;
        assert!(
            err.is_terminal(),
            "EmptyDeferConstraintsScope must be terminal"
        );
        assert!(
            !err.is_transient(),
            "EmptyDeferConstraintsScope must not be transient"
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("DeferScope::All"),
            "expected `DeferScope::All` remediation hint in message, got: {msg}"
        );
    }

    /// #169 (
    /// `ConflictingDeferrabilitySpec` mirrors the projection-time
    /// gate at runtime. The fix is at the model declaration; retrying
    /// cannot resolve the underlying inventory disagreement.
    #[test]
    fn conflicting_deferrability_spec_is_terminal_and_carries_payload() {
        let err = DjogiError::ConflictingDeferrabilitySpec {
            model_type_name: "Post".into(),
            field_name: "author_id".into(),
            first: (true, false),
            second: (true, true),
        };
        assert!(
            err.is_terminal(),
            "ConflictingDeferrabilitySpec must be terminal"
        );
        assert!(!err.is_transient());
        let msg = format!("{err}");
        assert!(
            msg.contains("Post.author_id"),
            "expected `model.field` in message, got: {msg}"
        );
        assert!(
            msg.contains("(true, false)") && msg.contains("(true, true)"),
            "expected both conflicting (deferrable, initially_deferred) tuples in message, got: {msg}"
        );
    }

    /// #169 (
    /// `OrphanDeferrabilitySpec` is terminal because the inventory
    /// shape is fixed at link time; no amount of retrying re-emits
    /// the missing `ModelDescriptor`.
    #[test]
    fn orphan_deferrability_spec_is_terminal_and_names_field() {
        let err = DjogiError::OrphanDeferrabilitySpec {
            model_type_name: "Ghost".into(),
            field_name: "haunts".into(),
        };
        assert!(err.is_terminal());
        assert!(!err.is_transient());
        let msg = format!("{err}");
        assert!(
            msg.contains("Ghost.haunts"),
            "expected `model.field` in message, got: {msg}"
        );
    }

    /// #169 (
    /// `DuplicateConstraintName` is terminal; the fix is at the
    /// model declaration, not at the retry site.
    #[test]
    fn duplicate_constraint_name_is_terminal_and_carries_both_fields() {
        let err = DjogiError::DuplicateConstraintName {
            constraint_name: "long_table_long_column_fkey".into(),
            first_model: "Alpha".into(),
            first_field: "ref".into(),
            second_model: "Beta".into(),
            second_field: "ref".into(),
        };
        assert!(err.is_terminal());
        assert!(!err.is_transient());
        let msg = format!("{err}");
        assert!(
            msg.contains("Alpha") && msg.contains("Beta"),
            "expected both colliding models in message, got: {msg}"
        );
        assert!(
            msg.contains("long_table_long_column_fkey"),
            "expected colliding constraint name in message, got: {msg}"
        );
    }

    #[test]
    fn unsupported_postgres_version_is_terminal() {
        let err = DjogiError::unsupported_postgres_version(16, 4, 18);
        assert!(
            err.is_terminal(),
            "UnsupportedPostgresVersion must be terminal"
        );
        assert!(
            !err.is_transient(),
            "UnsupportedPostgresVersion must not be transient"
        );
    }

    #[test]
    fn unsupported_postgres_version_displays_versions_and_upgrade() {
        let err = DjogiError::unsupported_postgres_version(16, 4, 18);
        let msg = format!("{err}");
        assert!(
            msg.contains("16.4"),
            "Display must name detected version, got: {msg}"
        );
        assert!(
            msg.contains("18"),
            "Display must name minimum version, got: {msg}"
        );
        assert!(
            msg.contains("upgrade") || msg.contains("Upgrade"),
            "Display must suggest upgrade, got: {msg}"
        );
    }

    #[test]
    fn unsupported_postgres_version_error_is_clear() {
        let err = DjogiError::unsupported_postgres_version(17, 0, 18);
        let msg = format!("{err}");
        assert!(
            msg.contains("PostgreSQL"),
            "Display must mention PostgreSQL, got: {msg}"
        );
        assert!(
            msg.contains("minimum") || msg.contains("below"),
            "Display must indicate minimum requirement, got: {msg}"
        );
    }
}
