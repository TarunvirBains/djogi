//! Sighting — observation events.
//!
//! Demonstrates:
//! - `GeoPoint` — spatial type with EWKB codec. Indexed (GIST) so the
//!   `nearby_sightings` and `cluster_sightings` demos run on real
//!   index plans, not seq scans.
//! - FTS on `notes`.
//! - Transactional outbox — `Sighting::create` enqueues a
//!   `SightingRecorded` event in the same transaction as the insert.
//!   The outbox worker (running separately) drains the event into
//!   whatever downstream consumer the operator wires up.
//!
//! Why no `created_at` field shown — the `#[model]` macro injects
//! `id`, `created_at`, `updated_at` for us. `observed_at` is the
//! domain-meaningful timestamp (when the sighting happened, not when
//! the row was inserted).

use djogi::prelude::*;
use time::OffsetDateTime;
use crate::models::{Elephant, Researcher};

#[model(
    table = "sightings",
    outbox(event = "SightingRecorded"),
)]
#[derive(Debug, Clone)]
pub struct Sighting {
    pub elephant: ForeignKey<Elephant>,

    pub observed_by: ForeignKey<Researcher>,

    /// EPSG:4326 point — longitude/latitude in WGS84.
    #[field(srid = 4326)]
    pub location: GeoPoint,

    pub observed_at: OffsetDateTime,

    /// FTS-indexed observation notes.
    #[field(fts = "english")]
    pub notes: String,
}
