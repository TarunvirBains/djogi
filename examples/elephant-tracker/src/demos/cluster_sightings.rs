//! `cluster-sightings` — DBSCAN-style spatial clustering over
//! `Sighting.location`.
//!
//! ## What this demonstrates
//!
//! Surfaces density hotspots in the seeded sighting data. Herds tend
//! to cluster around water sources, and the seed jitters sightings
//! within a small radius of three named water sources per herd, so a
//! density-based clustering pass should produce a small handful of
//! clusters per herd plus a noise bucket for outliers.
//!
//! ## Djogi typed surface
//!
//! This demo runs entirely through Djogi's typed surface — zero raw
//! SQL. The clustering uses `QuerySet::cluster_by_proximity` (Phase
//! 6.5), which produces a `GroupedQuerySet<Sighting, ClusterId>`
//! keyed by DBSCAN's cluster id (`ClusterId(None)` is the noise
//! bucket). Per-cluster reductions chain through `.annotate(...)`
//! using three typed aggregates from Cluster E:
//!
//! - `f.id().count_star()` → per-cluster sighting count.
//! - `f.location().centroid()` → per-cluster `GeoPoint` centroid,
//!   emitted as `ST_Centroid(ST_Collect(<col>::geometry))::geography`.
//! - `f.id().array_agg().order_by(f.id().asc())` → deterministic
//!   list of contributing sighting ids.
//!
//! Aggregation runs server-side in a single round trip; the Rust
//! side only decomposes the typed tuple `(ClusterId, i64, GeoPoint,
//! Vec<HeerId>)` into the demo's row shape.
//!
//! ## Output formats
//!
//! - `json` (default): a list of clusters, each with cluster id,
//!   sighting count, centroid latitude and longitude, and the list of
//!   contributing sighting ids.
//! - `markdown`: a sorted table over the same data.

use anyhow::Result;
use djogi::DjogiContext;
use djogi::prelude::*;
use djogi::query::ClusterRadius;
use serde::Serialize;
use std::path::Path;

use crate::models::Sighting;
use crate::output::{self, Format};

#[derive(Serialize, Clone)]
struct ClusterRow {
    cluster_id: Option<i32>,
    count: i64,
    centroid_lat: f64,
    centroid_lon: f64,
    sighting_ids: Vec<String>,
}

pub async fn run(ctx: &mut DjogiContext, format: Format, out: Option<&Path>) -> Result<()> {
    // DBSCAN with `eps = 50_000` metres (`ClusterRadius::meters`
    // routes through `ST_Transform(..., 3857)` so the radius is
    // interpreted in metres rather than degrees). `min_points = 3`
    // keeps isolated sightings in the noise bucket (`ClusterId(None)`).
    //
    // The radius is wide enough to merge sightings around the same
    // water source but tight enough to keep herds on different
    // continents in separate clusters.
    let mut rows = Sighting::objects()
        .cluster_by_proximity(
            |f| f.location(),
            ClusterRadius::meters(50_000.0).min_points(3),
        )
        .annotate(|f| {
            (
                f.id().count_star(),
                f.location().centroid(),
                f.id().array_agg().order_by(f.id().asc()),
            )
        })
        .fetch_all(ctx)
        .await?;

    // `cluster_by_proximity` does not pin the noise bucket
    // (`ClusterId(None)`) to the end of the result set — sort
    // here so output ordering matches the previous raw-SQL
    // `ORDER BY cluster_id NULLS LAST` shape.
    rows.sort_by(|a, b| match (a.0.0, b.0.0) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });

    let clusters: Vec<ClusterRow> = rows
        .into_iter()
        .map(|(cluster_id, (count, centroid, sighting_ids))| ClusterRow {
            cluster_id: cluster_id.0,
            count,
            centroid_lat: centroid.lat,
            centroid_lon: centroid.lon,
            sighting_ids: sighting_ids
                .iter()
                .map(|h| h.as_i64().to_string())
                .collect(),
        })
        .collect();

    let mut target = output::open_writer(out)?;
    match format {
        Format::Json => output::write_json(&mut target, &clusters)?,
        Format::Markdown => render_markdown(&mut target, &clusters)?,
        Format::Mermaid => {
            anyhow::bail!("cluster-sightings does not support --format mermaid")
        }
    }
    Ok(())
}

fn render_markdown(target: &mut output::OutputTarget, clusters: &[ClusterRow]) -> Result<()> {
    output::write_line(target, "# Sighting clusters\n")?;
    output::write_line(
        target,
        "| Cluster id | Count | Centroid lat | Centroid lon | Sighting ids |",
    )?;
    output::write_line(target, "|---|---:|---:|---:|---|")?;
    for c in clusters {
        let id_label = match c.cluster_id {
            Some(i) => i.to_string(),
            None => "noise".to_string(),
        };
        let ids = if c.sighting_ids.len() > 6 {
            format!(
                "{} … (+{} more)",
                c.sighting_ids[..6].join(", "),
                c.sighting_ids.len() - 6
            )
        } else {
            c.sighting_ids.join(", ")
        };
        output::write_line(
            target,
            &format!(
                "| {} | {} | {:.4} | {:.4} | {} |",
                id_label, c.count, c.centroid_lat, c.centroid_lon, ids
            ),
        )?;
    }
    Ok(())
}
