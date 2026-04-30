//! `lineage` — recursive-CTE descent from a named matriarch.
//!
//! ## What this demonstrates
//!
//! Djogi does not ship a tree-query API. When you need ancestor or
//! descendant traversal, the right move is to drop to raw SQL via
//! `ctx.raw_rows`, which is the always-available escape hatch for
//! shapes that fall outside the typed `QuerySet` surface. The demo
//! is honest about that — it shows the recursive CTE inline rather
//! than pretending every shape has a typed builder.
//!
//! ## Output formats
//!
//! - `json` (default): a flat list of
//!   `{depth, id, name, parent_id, parent_name, birth_year, sex}`.
//! - `mermaid`: `graph TD` with one edge per parent->child relation.
//! - `markdown`: the Mermaid block followed by an attribute table.

use anyhow::Result;
use djogi::DjogiContext;
use postgres_types::ToSql;
use serde::Serialize;
use std::path::Path;

use crate::output::{self, Format};

#[derive(Serialize, Clone)]
struct LineageRow {
    depth: i32,
    id: String,
    name: String,
    parent_id: Option<String>,
    parent_name: Option<String>,
    birth_year: Option<i16>,
    sex: Option<String>,
}

pub async fn run(
    ctx: &mut DjogiContext,
    matriarch: &str,
    max_depth: i32,
    format: Format,
    out: Option<&Path>,
) -> Result<()> {
    // Recursive CTE walks descendants level by level. The `depth`
    // bound stays inside the recursive arm so Postgres prunes early
    // rather than forming the entire tree before filtering.
    const SQL: &str = "WITH RECURSIVE descent AS (
            SELECT
                e.id,
                e.name,
                e.parent_id,
                e.estimated_birth_year,
                e.tags,
                0 AS depth
            FROM elephants e
            WHERE e.name = $1 AND e.parent_id IS NULL
            UNION ALL
            SELECT
                e.id,
                e.name,
                e.parent_id,
                e.estimated_birth_year,
                e.tags,
                d.depth + 1
            FROM elephants e
            JOIN descent d ON e.parent_id = d.id
            WHERE d.depth + 1 <= $2
        )
        SELECT
            d.depth          AS depth,
            d.id             AS id,
            d.name           AS name,
            d.parent_id      AS parent_id,
            p.name           AS parent_name,
            d.estimated_birth_year AS birth_year,
            d.tags->>'sex'   AS sex
        FROM descent d
        LEFT JOIN elephants p ON p.id = d.parent_id
        ORDER BY d.depth, d.name";
    let binds: &[&(dyn ToSql + Sync)] = &[&matriarch, &max_depth];
    let rows = ctx.raw_rows(SQL, binds).await?;

    let lineage: Vec<LineageRow> = rows
        .iter()
        .map(|row| LineageRow {
            depth: row.get::<_, i32>("depth"),
            id: row.get::<_, i64>("id").to_string(),
            name: row.get::<_, String>("name"),
            parent_id: row
                .get::<_, Option<i64>>("parent_id")
                .map(|v| v.to_string()),
            parent_name: row.get::<_, Option<String>>("parent_name"),
            birth_year: row.get::<_, Option<i16>>("birth_year"),
            sex: row.get::<_, Option<String>>("sex"),
        })
        .collect();

    let mut target = output::open_writer(out)?;
    match format {
        Format::Json => output::write_json(&mut target, &lineage)?,
        Format::Mermaid => render_mermaid(&mut target, matriarch, &lineage)?,
        Format::Markdown => render_markdown(&mut target, matriarch, &lineage)?,
    }
    Ok(())
}

fn render_mermaid(
    target: &mut output::OutputTarget,
    matriarch: &str,
    rows: &[LineageRow],
) -> Result<()> {
    output::write_line(target, "graph TD")?;
    if rows.is_empty() {
        let m = output::escape_label(matriarch);
        output::write_line(target, &format!("    n0[\"{m} (not found)\"]"))?;
        return Ok(());
    }
    for r in rows {
        let id = output::mermaid_node_id(r.id.parse::<i64>().unwrap_or(0));
        let label = output::escape_label(&r.name);
        output::write_line(target, &format!("    {id}[\"{label}\"]"))?;
    }
    for r in rows {
        let id = output::mermaid_node_id(r.id.parse::<i64>().unwrap_or(0));
        if let Some(pid) = &r.parent_id {
            let pid_node = output::mermaid_node_id(pid.parse::<i64>().unwrap_or(0));
            output::write_line(target, &format!("    {pid_node} --> {id}"))?;
        }
    }
    Ok(())
}

fn render_markdown(
    target: &mut output::OutputTarget,
    matriarch: &str,
    rows: &[LineageRow],
) -> Result<()> {
    output::write_line(target, &format!("# Lineage of {matriarch}\n"))?;
    if rows.is_empty() {
        output::write_line(
            target,
            &format!("_No matriarch named `{matriarch}` was found._"),
        )?;
        return Ok(());
    }
    output::write_line(target, "```mermaid")?;
    render_mermaid(target, matriarch, rows)?;
    output::write_line(target, "```\n")?;

    output::write_line(target, "| Depth | Name | Parent | Birth year | Sex |")?;
    output::write_line(target, "|---:|---|---|---:|---|")?;
    for r in rows {
        let parent = r.parent_name.as_deref().unwrap_or("—");
        let birth = r
            .birth_year
            .map(|y| y.to_string())
            .unwrap_or_else(|| "—".to_string());
        let sex = r.sex.as_deref().unwrap_or("—");
        output::write_line(
            target,
            &format!(
                "| {} | {} | {} | {} | {} |",
                r.depth, r.name, parent, birth, sex
            ),
        )?;
    }
    Ok(())
}
