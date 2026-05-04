//! Sighting — observation events recorded by field researchers.
//!
//! ## What this demonstrates
//!
//! - `GeoPoint` — spatial type with EWKB codec. The migration differ
//!   emits a `GEOGRAPHY(Point, 4326)` column and a GiST index on it, so
//!   the `cluster-sightings` and any `within_km` demos run on real index
//!   plans rather than sequential scans.
//! - Model-level FTS on `notes` — adopters get an `@@`-style match
//!   accelerator and a typed `SightingFields::search()` accessor without
//!   touching tsvector boilerplate by hand.
//! - Transactional outbox — `#[model(... events)]` causes every
//!   `Sighting::create` / `save` / `delete` to enqueue a row in
//!   `sightings_outbox` inside the same transaction as the data write.
//!   An external worker drains the queue into whatever downstream
//!   consumer the operator wires up. The example does not run the worker.
//! - `no_default` — both `ForeignKey` and `GeoPoint` lack `Default`, so
//!   the macro's `Default` derivation is suppressed.
//!
//! Why no `created_at` field is declared explicitly — the `#[model]`
//! macro injects `id`, `created_at`, and `updated_at` automatically.
//! `observed_at` is the domain-meaningful timestamp (when the sighting
//! happened, not when the row was inserted).

use crate::models::{Elephant, Herd, Researcher};
use djogi::prelude::*;
use time::OffsetDateTime;

#[model(
    table = "sightings",
    pk = HeerId,
    no_default,
    events,
    fts(source = "notes", dictionary = "english"),
)]
#[derive(Debug, Clone, Serialize)]
pub struct Sighting {
    pub elephant_id: ForeignKey<Elephant>,

    /// Denormalized herd FK — duplicates `elephant.herd_id` for query
    /// convenience. The mating-pairs demo's typed `Sighting::objects()
    /// .group_by(|s| s.herd_id()).annotate(|s| s.location().convex_hull())`
    /// path needs `herd_id` directly on Sighting; without this column,
    /// the typed `group_by` would have to traverse `s.elephant().herd_id()`,
    /// which the framework's grouped-aggregate surface doesn't model.
    /// Adopters writing similar by-group queries against an
    /// elephant-by-elephant observation log frequently denormalize the
    /// herd FK for the same reason. The column is `NOT NULL` and
    /// always set to `elephant.herd_id` at write time.
    pub herd_id: ForeignKey<Herd>,

    pub observed_by_id: ForeignKey<Researcher>,

    /// EPSG:4326 point — longitude/latitude in WGS84. Stored as a
    /// PostGIS `GEOGRAPHY(Point, 4326)` column with a GiST index.
    pub location: GeoPoint,

    pub observed_at: OffsetDateTime,

    /// Observation notes. Concatenated into the model-level `search`
    /// tsvector column by the FTS configuration above.
    pub notes: String,
}
