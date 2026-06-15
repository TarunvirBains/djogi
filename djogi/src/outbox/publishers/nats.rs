//! NATS publisher stub —.
//! Gated on the `outbox-nats` feature flag. The actual integration
//! including adding `async-nats` as a workspace dependency — ships in a
//! future task once the provider dependency matrix is decided. Today this
//! file provides a type-checked placeholder so the feature flag compiles
//! and the relay binary can reference `NatsPublisher` by name.

use crate::outbox::publisher::{PublishError, Publisher};
use crate::outbox::worker::OutboxRow;
use async_trait::async_trait;

/// Delivers outbox rows to a NATS subject (stub — not yet implemented).
/// Enable with `djogi = { features = ["outbox-nats"] }`. The full
/// implementation ships once the `async-nats` workspace dep is added.
pub struct NatsPublisher;

#[async_trait]
impl Publisher for NatsPublisher {
    async fn publish(&self, _row: &OutboxRow) -> Result<(), PublishError> {
        // Full NATS integration ships in a future task when the `async-nats`
        // workspace dep is added. Until then this panics to ensure accidental
        // production use is caught immediately.
        unimplemented!(
            "NatsPublisher is not yet implemented; \
    the full integration ships in a future task"
        )
    }
}
