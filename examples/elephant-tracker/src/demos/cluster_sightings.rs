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
//! Djogi's `spatial_grouping` module ships a `cluster_by_proximity`
//! grouping primitive that emits a per-row cluster id; this demo runs
//! the cluster ID assignment via raw SQL using PostGIS's `ST_ClusterDBSCAN`
//! window function so the demo stays readable in isolation.
//!
//! ## Output formats
//!
//! - `json` (default): a list of clusters, each with cluster id,
//!   sighting count, centroid latitude and longitude, and the list of
//!   contributing sighting ids.
//! - `markdown`: a sorted table over the same data.

use anyhow::Result;
use djogi::DjogiContext;
use postgres_types::ToSql;
use serde::Serialize;
use std::path::Path;

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
    // ST_ClusterDBSCAN runs as a window function and assigns each row
    // a cluster id (NULL = noise). The outer query aggregates per
    // cluster id, computes the centroid, and gathers the contributing
    // sighting ids.
    //
    // The geography column is reprojected to EPSG:3857 (Web Mercator)
    // before clustering so DBSCAN's `eps` parameter is interpreted in
    // metres rather than degrees. 50_000 m is wide enough to merge
    // sightings around the same water source but tight enough to keep
    // herds on different continents in separate clusters.
    const SQL: &str = "WITH clustered AS (
            SELECT
                id,
                location::geometry AS geom,
                ST_ClusterDBSCAN(
                    ST_Transform(location::geometry, 3857),
                    eps := 50000,
                    minpoints := 3
                ) OVER () AS cluster_id
            FROM sightings
        )
        SELECT
            cluster_id,
            COUNT(*)::BIGINT                   AS count,
            ST_Y(ST_Centroid(ST_Collect(geom))) AS centroid_lat,
            ST_X(ST_Centroid(ST_Collect(geom))) AS centroid_lon,
            ARRAY_AGG(id ORDER BY id)          AS sighting_ids
        FROM clustered
        GROUP BY cluster_id
        ORDER BY cluster_id NULLS LAST";

    let rows = ctx.raw_rows(SQL, &[] as &[&(dyn ToSql + Sync)]).await?;
    let clusters: Vec<ClusterRow> = rows
        .iter()
        .map(|row| ClusterRow {
            cluster_id: row.get::<_, Option<i32>>("cluster_id"),
            count: row.get::<_, i64>("count"),
            centroid_lat: row.get::<_, f64>("centroid_lat"),
            centroid_lon: row.get::<_, f64>("centroid_lon"),
            sighting_ids: row
                .get::<_, Vec<i64>>("sighting_ids")
                .into_iter()
                .map(|i| i.to_string())
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
