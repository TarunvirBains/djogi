//! `cross-border-herds` — herds whose ranges span >=2 countries within
//! the same season.
//!
//! ## What this demonstrates
//!
//! Walks the `Herd ↔ Country` many-to-many via the `HerdRange` through
//! model. The query is a single SQL pass that joins through
//! `herd_ranges`; the result is the load-bearing reason for `HerdRange`
//! to carry the `season` payload at all (a payload-free junction would
//! erase the seasonal cross-border story).
//!
//! ## Output formats
//!
//! - `json` (default): one entry per (herd, season) pair that crosses
//!   a border, listing the country names visited that season.
//! - `mermaid`: `graph LR` with one edge per (herd, country, season)
//!   pair, edge label = `season`.
//! - `markdown`: per-herd sections combining the Mermaid block and a
//!   range table.

use anyhow::Result;
use djogi::DjogiContext;
use postgres_types::ToSql;
use serde::Serialize;
use std::path::Path;

use crate::output::{self, Format};

#[derive(Serialize, Clone)]
struct CrossBorderEntry {
    herd_id: String,
    herd_name: String,
    season: String,
    countries: Vec<String>,
}

pub async fn run(ctx: &mut DjogiContext, format: Format, out: Option<&Path>) -> Result<()> {
    // The aggregating query: for every (herd, season) pair, collect the
    // distinct country names. The `HAVING` clause filters down to the
    // pairs that span more than one country.
    //
    // We use raw SQL via `ctx.raw_rows` because the typed `QuerySet`
    // surface for grouped aggregation through an explicit-through M2M
    // is in flux as of this example. The raw form is small, readable,
    // and pins the demo to a stable SQL contract.
    const SQL: &str = "SELECT
            h.id           AS herd_id,
            h.name         AS herd_name,
            hr.season      AS season,
            ARRAY_AGG(c.name ORDER BY c.name) AS countries
        FROM herds h
        JOIN herd_ranges hr ON hr.herd_id = h.id
        JOIN countries c ON c.id = hr.country_id
        GROUP BY h.id, h.name, hr.season
        HAVING COUNT(DISTINCT c.id) >= 2
        ORDER BY h.name, hr.season";
    let rows = ctx.raw_rows(SQL, &[] as &[&(dyn ToSql + Sync)]).await?;

    let entries: Vec<CrossBorderEntry> = rows
        .iter()
        .map(|row| CrossBorderEntry {
            herd_id: row.get::<_, i64>("herd_id").to_string(),
            herd_name: row.get::<_, String>("herd_name"),
            season: row.get::<_, String>("season"),
            countries: row.get::<_, Vec<String>>("countries"),
        })
        .collect();

    let mut target = output::open_writer(out)?;
    match format {
        Format::Json => output::write_json(&mut target, &entries)?,
        Format::Mermaid => render_mermaid(&mut target, &entries)?,
        Format::Markdown => render_markdown(&mut target, &entries)?,
    }
    Ok(())
}

fn render_mermaid(target: &mut output::OutputTarget, entries: &[CrossBorderEntry]) -> Result<()> {
    output::write_line(target, "graph LR")?;
    for e in entries {
        let herd_node = output::mermaid_node_id_from_str(&e.herd_id);
        let herd_label = output::escape_label(&e.herd_name);
        for country in &e.countries {
            // Country nodes use a hash of the country name so the same
            // country name across multiple herds collapses to one node.
            let country_id = country_node_id(country);
            let country_label = output::escape_label(country);
            output::write_line(target, &format!("    {herd_node}[\"{herd_label}\"]"))?;
            output::write_line(target, &format!("    {country_id}[\"{country_label}\"]"))?;
            let season_label = output::escape_label(&e.season);
            output::write_line(
                target,
                &format!("    {herd_node} -->|{season_label}| {country_id}"),
            )?;
        }
    }
    Ok(())
}

fn render_markdown(target: &mut output::OutputTarget, entries: &[CrossBorderEntry]) -> Result<()> {
    output::write_line(target, "# Cross-border herds\n")?;
    if entries.is_empty() {
        output::write_line(target, "_No cross-border herds in the seed data._")?;
        return Ok(());
    }

    // Group by herd name for the per-herd sections.
    let mut by_herd: std::collections::BTreeMap<&str, Vec<&CrossBorderEntry>> = Default::default();
    for e in entries {
        by_herd.entry(&e.herd_name).or_default().push(e);
    }
    for (herd_name, group) in by_herd {
        output::write_line(target, &format!("## {herd_name}\n"))?;
        output::write_line(target, "```mermaid")?;
        let herd_node = output::mermaid_node_id(group[0].herd_id.parse::<i64>().unwrap_or(0));
        let herd_label = output::escape_label(herd_name);
        output::write_line(target, "graph LR")?;
        output::write_line(target, &format!("    {herd_node}[\"{herd_label}\"]"))?;
        for e in &group {
            for country in &e.countries {
                let cid = country_node_id(country);
                let clabel = output::escape_label(country);
                let season = output::escape_label(&e.season);
                output::write_line(target, &format!("    {cid}[\"{clabel}\"]"))?;
                output::write_line(target, &format!("    {herd_node} -->|{season}| {cid}"))?;
            }
        }
        output::write_line(target, "```\n")?;
        output::write_line(target, "| Season | Countries |")?;
        output::write_line(target, "|---|---|")?;
        for e in &group {
            output::write_line(
                target,
                &format!("| {} | {} |", e.season, e.countries.join(", ")),
            )?;
        }
        output::write_line(target, "")?;
    }
    Ok(())
}

/// Stable Mermaid node id derived from a country name. The id must be
/// a valid Mermaid identifier; we prefix `c` and append the FNV-1a
/// 32-bit hash. No regex involved.
fn country_node_id(name: &str) -> String {
    let mut h: u32 = 0x811c9dc5;
    for byte in name.as_bytes() {
        h ^= *byte as u32;
        h = h.wrapping_mul(0x01000193);
    }
    format!("c{h:x}")
}
