//! Programmatic seed — herds, herd-ranges, elephants, sightings.
//!
//! Run after `seeds/countries.sql`. Generates:
//! - 4 herds (Amboseli-A, Maasai-Mara-B, Selous-C, Hwange-D)
//! - HerdRange rows that span ≥2 countries per herd in at least one
//!   season (so the cross-border demo has data)
//! - ~30 elephants per herd with a mix of `parent` set / `None` so
//!   the lineage demo finds 2-3 levels of descendants
//! - ~50 sightings per herd, GeoPoint-distributed in clusters near
//!   water sources so the cluster_sightings demo's DBSCAN finds real
//!   density hotspots rather than uniform noise
//!
//! Why programmatic and not SQL: writing 200 rows of `INSERT INTO
//! sightings (location, ...) VALUES (ST_GeomFromEWKB(...), ...)` by
//! hand would obscure the GeoPoint typed-constructor story this seed
//! is meant to demonstrate.

use anyhow::Result;
use djogi::prelude::*;

pub async fn run(ctx: &DjogiContext) -> Result<()> {
    todo!("seed herds, herd_ranges, elephants, sightings")
}
