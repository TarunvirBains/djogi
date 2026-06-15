//! `values-scores` — join a process-local score list to model rows.
//!
//! ## What this demonstrates
//!
//! Uses [`InlineValues`] to join a `Vec<(djogi::HeerId, f64)>` score list computed
//! in Rust against the `Elephant` model table using a typed `ON` predicate.
//! No raw SQL is used anywhere in the join or decode path.
//!
//! ## SQL shape produced
//!
//! ```sql
//! SELECT
//!  __djogi_m.id   AS id,
//!  __djogi_m.name  AS name,
//! ...,
//!  scores.elephant_id AS __djogi_values_0,
//!  scores.score   AS __djogi_values_1
//! FROM elephants AS __djogi_m
//! INNER JOIN (VALUES
//!  ($1::BIGINT, $2::DOUBLE PRECISION),
//!  ($3, $4),
//! ...
//! ) AS scores(elephant_id, score)
//! ON __djogi_m.id = scores.elephant_id
//! ORDER BY __djogi_m.name ASC
//! ```
//!
//! ## Output formats
//!
//! - `json` (default): array of `{id, name, score}` objects.
//! - `markdown`: table with id, name, score columns.
//!
//! ## Chunking note
//!
//! Very large score lists (thousands of entries) should be loaded into a
//! staging table rather than sent as `VALUES`. Postgres can plan large
//! `VALUES` clauses poorly. For lists up to ~500 rows, the inline approach
//! is efficient and avoids extra round trips.

use anyhow::Result;
use djogi::DjogiContext;
use djogi::prelude::*;
use serde::Serialize;
use std::path::Path;

use crate::models::Elephant;
use crate::output::{self, Format};

#[derive(Serialize)]
struct Row {
    id: String,
    name: String,
    score: f64,
}

/// Run the `values-scores` demo.
///
/// Joins a hardcoded score list against the `Elephant` table and prints the
/// matched pairs in the requested format.
///
/// In a real application the score list would be computed at runtime from a
/// recommendation engine, a ranking query, or any other Rust-side algorithm.
pub async fn run(ctx: &mut DjogiContext, format: Format, out: Option<&Path>) -> Result<()> {
    // Fetch all elephants so we have their IDs.
    let elephants: Vec<Elephant> = Elephant::objects()
        .order_by(|f| f.name().asc())
        .fetch_all(ctx)
        .await?;

    if elephants.is_empty() {
        eprintln!("No elephants found — seed the database first.");
        return Ok(());
    }

    // Build a synthetic score list using the first few elephants.
    // In production this would come from an algorithm, not a literal.
    let scores: Vec<(djogi::HeerId, f64)> = elephants
        .iter()
        .take(3)
        .enumerate()
        .map(|(i, e)| (e.id, 0.95 - (i as f64) * 0.10))
        .collect();

    // Construct the typed inline-values relation.
    let weights: InlineValues<(djogi::HeerId, f64)> =
        InlineValues::new(scores, "scores", ("elephant_id", "score"))?;

    // JOIN the score list against the Elephant table.
    // `eq_values(v.col0())` links `Elephant::id` (BIGINT) to the first
    // VALUES column `elephant_id` (also BIGINT) — mismatched types would be
    // a compile error.
    let pairs: Vec<(Elephant, (djogi::HeerId, f64))> = Elephant::objects()
        .order_by(|f| f.name().asc())
        .join_values(weights, |e, v| e.id().eq_values(v.col0()))
        .fetch_all(ctx)
        .await?;

    let rows: Vec<Row> = pairs
        .into_iter()
        .map(|(elephant, (_id, score))| Row {
            id: elephant.id.as_i64().to_string(), // HeerId → i64 → String
            name: elephant.name.to_string(),
            score,
        })
        .collect();

    let mut target = output::open_writer(out)?;
    match format {
        Format::Json => output::write_json(&mut target, &rows)?,
        Format::Markdown => render_markdown(&mut target, &rows)?,
        Format::Mermaid => anyhow::bail!("values-scores does not support --format mermaid"),
    }
    Ok(())
}

fn render_markdown(target: &mut output::OutputTarget, rows: &[Row]) -> Result<()> {
    output::write_line(target, "# Elephant scores\n")?;
    output::write_line(target, "| ID | Name | Score |")?;
    output::write_line(target, "|---|---|---:|")?;
    for r in rows {
        output::write_line(
            target,
            &format!("| {} | {} | {:.2} |", r.id, r.name, r.score),
        )?;
    }
    Ok(())
}
