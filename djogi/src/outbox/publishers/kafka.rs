//! Kafka publisher stub —.
//! Gated on the `outbox-kafka` feature flag. The actual integration
//! including adding `rdkafka` as a workspace dependency — ships in a future
//! task once the provider dependency matrix is decided. Today this file
//! provides a type-checked placeholder so the feature flag compiles and
//! the relay binary can reference `KafkaPublisher` by name.

use crate::outbox::publisher::{PublishError, Publisher};
use crate::outbox::worker::OutboxRow;
use async_trait::async_trait;

/// Delivers outbox rows to a Kafka topic (stub — not yet implemented).
/// Enable with `djogi = { features = ["outbox-kafka"] }`. The full
/// implementation ships once the `rdkafka` workspace dep is added.
pub struct KafkaPublisher;

#[async_trait]
impl Publisher for KafkaPublisher {
    async fn publish(&self, _row: &OutboxRow) -> Result<(), PublishError> {
        // Full Kafka integration ships in a future task when the `rdkafka`
        // workspace dep is added. Until then this panics to ensure accidental
        // production use is caught immediately.
        unimplemented!(
            "KafkaPublisher is not yet implemented; \
    the full integration ships in a future task"
        )
    }
}
