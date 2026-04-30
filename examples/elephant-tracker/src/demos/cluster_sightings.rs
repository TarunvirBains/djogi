//! `cluster-sightings` demo — DBSCAN-style spatial clustering over
//! `Sighting.location`.
//!
//! Djogi's `spatial_grouping` module exposes geohash and density-based
//! clustering primitives that emit a per-row cluster id. We use the
//! grouping API to find density hotspots in the seeded data — herds
//! tend to cluster around water sources in dry season, so the seed data
//! distributes sightings non-uniformly to make the cluster output
//! look real.

use anyhow::Result;
use djogi::prelude::*;
use crate::models::Sighting;

pub async fn run(ctx: &DjogiContext) -> Result<()> {
    // Sketch — wired against real APIs once cluster PRs land.
    //
    //     let clusters = Sighting::objects()
    //         .filter(observed_at__gt(seven_days_ago))
    //         .cluster_by_location(ClusterRadius::km(5.0))
    //         .order_by_size_desc()
    //         .fetch_all(ctx)
    //         .await?;
    //     for c in clusters {
    //         println!("cluster {} — {} sightings, centroid {:?}",
    //                  c.id, c.count, c.centroid);
    //     }
    todo!("wire DBSCAN-style cluster query")
}
