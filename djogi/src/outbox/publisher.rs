//! The `Publisher` trait and `PublishError` enum — Phase 5 Task 11.5.
//!
//! Any struct that can deliver an outbox row to a downstream transport
//! implements `Publisher`. The framework ships one built-in implementation
//! ([`NotifyPublisher`](super::publishers::notify::NotifyPublisher)) that uses
//! Postgres `NOTIFY` with zero extra dependencies.
//!
//! # Object safety
//!
//! `Publisher` is designed to be stored as `Box<dyn Publisher>` or
//! `Arc<dyn Publisher>`. The `#[async_trait::async_trait]` attribute rewrites
//! the `async fn publish` body so the trait is object-safe.
//!
//! # Error variants
//!
//! `PublishError` carries a `Transient` / `Permanent` distinction so the relay
//! loop can decide whether to retry:
//!
//! - [`PublishError::Transient`] — downstream hiccup (network timeout, rate
//!   limit, broker restart). Pass `retryable = true` to `mark_failed`.
//! - [`PublishError::Permanent`] — payload too large, schema violation, unknown
//!   channel. Pass `retryable = false` to `mark_failed`.
//! - [`PublishError::Provider`] — wraps an underlying provider error where the
//!   transience is unknown; the relay should treat this as transient by default.

use super::worker::OutboxRow;
use async_trait::async_trait;

/// Deliver a single outbox row to a downstream transport.
///
/// Implementations must be `Send + Sync + 'static` so they can be stored in a
/// `Box<dyn Publisher>` and shared across Tokio tasks.
///
/// # Error semantics
///
/// - Return `Ok(())` only after the transport has durably accepted the message
///   (or the implementation is fire-and-forget by design).
/// - Return `Err(PublishError::Transient { .. })` for retriable failures so the
///   relay can call `mark_failed(retryable = true)` and try again later.
/// - Return `Err(PublishError::Permanent { .. })` for terminal failures so the
///   relay can call `mark_failed(retryable = false)`.
#[async_trait]
pub trait Publisher: Send + Sync + 'static {
    /// Publish one outbox row. Called by the relay loop after `claim_pending`
    /// returns the row.
    async fn publish(&self, row: &OutboxRow) -> Result<(), PublishError>;
}

/// Failure mode for a single publish attempt.
///
/// The relay loop inspects this to decide whether to retry via
/// `mark_failed(retryable = true/false)`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PublishError {
    /// Transient failure — the downstream is temporarily unavailable or slow.
    /// The relay should retry by calling `mark_failed(retryable = true)`.
    #[error("transient: {message}")]
    Transient {
        /// Human-readable description of what went wrong, stored in
        /// `failed_reason` for observability.
        message: String,
    },

    /// Permanent failure — the message cannot be delivered regardless of
    /// retries (schema violation, payload too large, unknown channel, etc.).
    /// The relay should call `mark_failed(retryable = false)`.
    #[error("permanent: {message}")]
    Permanent {
        /// Human-readable description. Stored in `failed_reason`.
        message: String,
    },

    /// An error from the underlying provider where transience is not known.
    ///
    /// The relay should treat this as transient (retry) unless the retry budget
    /// is exhausted. The inner source error is preserved for `std::error::Error`
    /// chain navigation.
    #[error("provider error: {0}")]
    Provider(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}
