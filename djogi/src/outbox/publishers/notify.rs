//! Postgres `NOTIFY`-based publisher — Phase 5 Task 11.5.
//!
//! `NotifyPublisher` delivers outbox rows by issuing `SELECT pg_notify($1, $2)`
//! over a regular database connection. It requires no external broker and is
//! available whenever the `outbox` feature is enabled.
//!
//! # Channel routing
//!
//! The Phase 5 plan's `NotifyPublisher` snippet used `row.model_table` to build
//! the channel name per-row. `OutboxRow` does not carry a `model_table` field
//! (the worker outbox is a generic table and rows do not remember their source
//! table beyond the outbox table name itself). Rather than adding that field
//! to `OutboxRow`, `NotifyPublisher` accepts a fixed `channel` string at
//! construction time and the reference relay binary builds one publisher per
//! outbox table with `format!("{channel_prefix}{table}")` — keeping per-table
//! NOTIFY routing while leaving `OutboxRow` minimal. Callers that need
//! finer-grained routing (e.g. per-action channels) create additional
//! publishers the same way.
//!
//! # Usage
//!
//! ```ignore
//! let publisher = NotifyPublisher::new(pool.clone(), "orders_outbox".to_string());
//! ```
//!
//! The channel name is embedded verbatim in `SELECT pg_notify($1, $2)` as a
//! bind parameter, so it is safe regardless of content.
//!
//! # Message payload
//!
//! The NOTIFY payload is the JSON string representation of `OutboxRow::payload`.
//! Subscribers receive a raw JSON string they can parse independently. Postgres
//! NOTIFY payloads are limited to 8000 bytes; payloads exceeding this limit will
//! silently truncate on the wire. For large payloads, use a broker-backed
//! publisher instead.

use crate::context::DjogiContext;
use crate::outbox::publisher::{PublishError, Publisher};
use crate::outbox::worker::OutboxRow;
use crate::pg::pool::DjogiPool;
use async_trait::async_trait;

/// Delivers outbox rows via Postgres `NOTIFY`.
///
/// Uses the pool to acquire a fresh connection per publish call. This is
/// intentional: the relay loop typically calls `mark_published` on the same
/// context as the claim, but `publish` should not run inside the same
/// transaction (NOTIFY delivery semantics are per-connection, not per-transaction
/// — the listener on a separate connection sees the notification when the
/// publishing transaction commits, or immediately if outside a transaction).
///
/// # Constructor
///
/// ```ignore
/// let publisher = NotifyPublisher::new(pool, "my_outbox_channel".to_string());
/// ```
///
/// If you need per-action channels, create multiple publishers:
///
/// ```ignore
/// let create_publisher = NotifyPublisher::new(pool.clone(), "orders_created".to_string());
/// let delete_publisher = NotifyPublisher::new(pool.clone(), "orders_deleted".to_string());
/// ```
pub struct NotifyPublisher {
    pool: DjogiPool,
    /// The Postgres channel name passed to `pg_notify`. Set at construction.
    channel: String,
}

impl NotifyPublisher {
    /// Create a new `NotifyPublisher`.
    ///
    /// - `pool` — the connection pool to acquire a connection from on each
    ///   `publish` call.
    /// - `channel` — the Postgres channel name (first argument to
    ///   `pg_notify`). Passed as a bind parameter, not interpolated into
    ///   SQL, so arbitrary content is safe.
    pub fn new(pool: DjogiPool, channel: String) -> Self {
        Self { pool, channel }
    }
}

#[async_trait]
impl Publisher for NotifyPublisher {
    async fn publish(&self, row: &OutboxRow) -> Result<(), PublishError> {
        let payload = row.payload.to_string();
        let mut ctx = DjogiContext::from_pool(self.pool.clone());
        ctx.raw_execute(
            "SELECT pg_notify($1, $2)",
            &[&self.channel.as_str(), &payload.as_str()],
        )
        .await
        .map_err(|e| PublishError::Provider(Box::new(e)))?;
        Ok(())
    }
}
