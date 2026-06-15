//! Redis publisher stub —.
//! Gated on the `outbox-redis` feature flag. The actual integration
//! including adding `redis` as a workspace dependency — ships in a future
//! task once the provider dependency matrix is decided. Today this file
//! provides a type-checked placeholder so the feature flag compiles and
//! the relay binary can reference `RedisPublisher` by name.

use crate::outbox::publisher::{PublishError, Publisher};
use crate::outbox::worker::OutboxRow;
use async_trait::async_trait;

/// Delivers outbox rows to a Redis channel (stub — not yet implemented).
/// Enable with `djogi = { features = ["outbox-redis"] }`. The full
/// implementation ships once the `redis` crate dependency is added to the
/// workspace.
pub struct RedisPublisher;

#[async_trait]
impl Publisher for RedisPublisher {
    async fn publish(&self, _row: &OutboxRow) -> Result<(), PublishError> {
        // Full Redis integration ships in a future task when the `redis`
        // workspace dep is added. Until then this panics to ensure accidental
        // production use is caught immediately.
        unimplemented!(
            "RedisPublisher is not yet implemented; \
    the full integration ships in a future task"
        )
    }
}
