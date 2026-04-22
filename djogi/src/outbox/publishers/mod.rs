//! Concrete `Publisher` implementations — Phase 5 Task 11.5.
//!
//! # Available publishers
//!
//! | Publisher | Feature flag | Status |
//! |-----------|-------------|--------|
//! | [`NotifyPublisher`] | `outbox` (always included) | Fully implemented |
//! | `RedisPublisher` | `outbox-redis` | Stub — `unimplemented!()` |
//! | `KafkaPublisher` | `outbox-kafka` | Stub — `unimplemented!()` |
//! | `NatsPublisher` | `outbox-nats` | Stub — `unimplemented!()` |
//!
//! Full Redis/Kafka/NATS integrations (including the provider crate deps) ship
//! in a future task once the provider dependency matrix is decided. The stubs
//! guarantee the feature flags compile today and give the relay binary a
//! type-checked placeholder to switch on.

pub mod notify;

#[cfg(feature = "outbox-redis")]
pub mod redis;

#[cfg(feature = "outbox-kafka")]
pub mod kafka;

#[cfg(feature = "outbox-nats")]
pub mod nats;

// Convenience re-exports so callers can write
// `djogi::outbox::publishers::NotifyPublisher` without the sub-path.
pub use notify::NotifyPublisher;
