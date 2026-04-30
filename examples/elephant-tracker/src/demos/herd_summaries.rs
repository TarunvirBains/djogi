//! `herd-summaries` — visage projection plus side-query trait.
//!
//! ## What this demonstrates
//!
//! Loads every `Herd` row, builds a `HerdSummary` projection from each,
//! and asks each summary for its live `herd_size` via the trait method
//! defined in `crate::visages::herd_summary`.
//!
//! Two things to notice when reading the code:
//!
//! 1. The summary projection is constructed without a database round
//!    trip beyond the initial `Herd` fetch. Visages are pure
//!    projections.
//! 2. The aggregate (`herd_size`) is opt-in per call site — adopters
//!    pay for the side query exactly when they want the count, never
//!    on every read of the summary.
//!
//! ## Output formats
//!
//! - `json` (default): a flat list of
//!   `{id, name, estimated_population, actual_size}`.
//! - `markdown`: a table with the same columns plus a `delta` column
//!   showing `actual_size - estimated_population`, useful for spotting
//!   herds that have outgrown their last census.

use anyhow::Result;
use djogi::DjogiContext;
use serde::Serialize;
use std::path::Path;

use crate::models::Herd;
use crate::output::{self, Format};
use crate::visages::{HerdSizeQuery, HerdSummary};

#[derive(Serialize)]
struct Row {
    id: String,
    name: String,
    estimated_population: i32,
    actual_size: i64,
}

pub async fn run(ctx: &mut DjogiContext, format: Format, out: Option<&Path>) -> Result<()> {
    let herds: Vec<Herd> = ctx
        .raw_query("SELECT * FROM herds ORDER BY name", &[])
        .await?;

    let mut rows = Vec::with_capacity(herds.len());
    for h in &herds {
        let summary = HerdSummary::from(h);
        let size = summary.herd_size(ctx).await?;
        rows.push(Row {
            id: summary.id.as_i64().to_string(),
            name: summary.name,
            estimated_population: summary.estimated_population,
            actual_size: size,
        });
    }

    let mut target = output::open_writer(out)?;
    match format {
        Format::Json => output::write_json(&mut target, &rows)?,
        Format::Markdown => render_markdown(&mut target, &rows)?,
        Format::Mermaid => {
            anyhow::bail!("herd-summaries does not support --format mermaid")
        }
    }
    Ok(())
}

fn render_markdown(target: &mut output::OutputTarget, rows: &[Row]) -> Result<()> {
    output::write_line(target, "# Herd summaries\n")?;
    output::write_line(
        target,
        "| Herd | Estimated population | Actual size | Delta |",
    )?;
    output::write_line(target, "|---|---:|---:|---:|")?;
    for r in rows {
        let delta = r.actual_size - r.estimated_population as i64;
        output::write_line(
            target,
            &format!(
                "| {} | {} | {} | {:+} |",
                r.name, r.estimated_population, r.actual_size, delta
            ),
        )?;
    }
    Ok(())
}
