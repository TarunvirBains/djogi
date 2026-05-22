//! `ReturningPair<T>` — the public result type for PostgreSQL 18 `OLD`/`NEW`
//! `RETURNING` on `UPDATE` statements.
//!
//! # PostgreSQL 18 background
//!
//! PostgreSQL 18 extended `RETURNING` to support per-row old and new snapshots:
//!
//! ```sql
//! UPDATE posts SET title = $1
//! RETURNING WITH (OLD AS __djogi_old, NEW AS __djogi_new)
//!   __djogi_old.id AS "__djogi_old__id", ...,
//!   __djogi_new.id AS "__djogi_new__id", ...
//! ```
//!
//! `ReturningPair<T>` carries both snapshots as fully-typed model instances.
//! Djogi emits the projection using its `__djogi_` reserved namespace and
//! decodes both sides through
//! [`FromJoinedPgRow`](crate::pg::decode::FromJoinedPgRow) with the prefixes
//! `"__djogi_old__"` and `"__djogi_new__"`.
//!
//! # Why PostgreSQL 18 only
//!
//! Djogi has a hard PostgreSQL 18 floor. No fallback, CTE polyfill, or
//! trigger-backed emulation is provided — PG18's native syntax is the only
//! supported path.
//!
//! # Scope
//!
//! `ReturningPair<T>` covers **UPDATE only**. DELETE exposes the old row as a
//! plain `T` (the `OLD` side only; the `NEW` side is absent by definition). INSERT
//! has no non-null `OLD` side in the normal case and is not modelled here; see
//! `Model::create` for the INSERT result type.
//!
//! # Protected fields
//!
//! Both `old` and `new` contain full model-field values, including any fields
//! annotated `#[field(protected(...))]`. The pair-returning APIs do not implement
//! field-level redaction — that policy is deferred to issue #227. Adopters who log
//! or persist a `ReturningPair<T>` are responsible for handling protected-data
//! exposure until #227 defines a framework-level redaction surface.

/// A before/after snapshot pair returned by PostgreSQL 18 `OLD`/`NEW`
/// `RETURNING` on an `UPDATE` statement.
///
/// Both sides are non-null full-model instances decoded from the database:
/// `old` is the row's state immediately before the `UPDATE` (including any
/// `BEFORE UPDATE` trigger effects on the old values), and `new` is the state
/// after the `UPDATE` and all trigger effects.
///
/// # Usage
///
/// ```ignore
/// use djogi::prelude::*;
///
/// let pair = post.update_returning_pair(&mut ctx).await?;
/// // pair.old — what was in the database before the update.
/// // pair.new — what is in the database after the update.
/// assert!(pair.new.updated_at >= pair.old.updated_at);
/// ```
///
/// # Consuming `self`
///
/// [`Model::update_returning_pair`](crate::model::Model::update_returning_pair)
/// consumes `self`. This is intentional — the caller's in-memory value is stale
/// after the update, and ownership transfer at the type level prevents accidental
/// reuse. Use `pair.new` to continue working with the updated row.
///
/// # Bulk variant
///
/// For bulk updates see
/// [`UpdateStmt::execute_returning_pairs`](crate::query::update::UpdateStmt::execute_returning_pairs).
/// That method materializes one `ReturningPair<T>` per affected row into a
/// `Vec<ReturningPair<T>>` — read the rustdoc warning on that method about
/// unbounded memory consumption.
///
/// # Protected fields
///
/// Both sides expose full model-field values, including
/// `#[field(protected(...))]` fields. Framework-level redaction is deferred to
/// issue #227. Log and persist pairs with care.
#[must_use = "inspect both old and new snapshots or explicitly drop the pair"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturningPair<T> {
    /// Row state immediately before the `UPDATE` (and any `BEFORE UPDATE`
    /// trigger effects on the old row).
    pub old: T,
    /// Row state after the `UPDATE` and all trigger effects — the value to
    /// continue working with.
    pub new: T,
}
