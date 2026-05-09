//! `lineage` — matrilineal descent from a named matriarch via raw
//! recursive-CTE SQL **or** the typed Cluster B builder.
//!
//! ## What this demonstrates
//!
//! Two paths to the same data shape — the demo's `--typed` flag
//! switches between them so adopters can read both side-by-side:
//!
//! ### Default mode — raw recursive-CTE SQL via `ctx.raw_rows`
//!
//! Single-edge matrilineal descent rendered via raw SQL. The
//! canonical escape hatch when you want to keep the SQL inline for
//! readability — matriarchal society biology is naturally a
//! single-edge walk through `mother_id`, and the recursive CTE is
//! short enough that adopters benefit from seeing it written out.
//!
//! ### Typed mode — `Elephant::objects().tree_descendants(ElephantRelated::mother(), id)`
//!
//! Pass `--typed` to switch to Phase 8-Zero Cluster B's typed
//! tree-walk builder. Compose with `--order=bfs|dfs` to lower into
//! `SEARCH BREADTH FIRST BY estimated_birth_year` /
//! `SEARCH DEPTH FIRST BY estimated_birth_year` on the recursive CTE
//! — clean top-down generation bands (BFS) or matriline-chain walks
//! (DFS). The `--order=default` mode skips the SEARCH clause and
//! lets Postgres pick the traversal order. Same single-edge
//! `mother_id` direction as the raw mode; the typed builder uses
//! the explicit `tree_descendants(edge, root_id)` form rather than
//! the inherent `Model::tree_descendants(id)` sugar because
//! `Elephant` declares two self-FKs and we don't want to bias the
//! model toward one edge for the mating-pairs demo's sake.
//!
//! ### Multi-edge ancestry
//!
//! Multi-edge (mother + father) ancestry lands in the
//! `mating-pairs` demo via the materialized `ElephantAncestry`
//! closure (populated at seed time by
//! `Model::materialize_closure::<ElephantAncestry>`). That closure
//! walks both self-FK edges in one recursive CTE while preserving
//! Wright path multiplicity, then mating-pairs joins the closure
//! to itself on `ancestor_id` for indexed shared-ancestor lookup
//! per candidate pair.
//!
//! ## Output formats
//!
//! - `json` (default): a flat list of
//!   `{depth, id, name, mother_id, mother_name, birth_year, sex}`.
//! - `mermaid`: `graph TD` with one edge per mother->child relation.
//! - `markdown`: the Mermaid block followed by an attribute table.

use anyhow::{Context, Result};
use clap::ValueEnum;
use djogi::__bypass::RawAccessExt as _;
use djogi::DjogiContext;
use djogi::prelude::*;
use postgres_types::ToSql;
use serde::Serialize;
use std::path::Path;

use crate::models::Elephant;
use crate::models::elephant::{ElephantFields, ElephantRelated};
use crate::output::{self, Format};

/// Traversal order for `lineage --typed` mode. Maps onto the
/// framework's `RecursiveQuerySet::search_breadth_first_by` /
/// `search_depth_first_by` builders (Phase 8-Zero Cluster B), which
/// emit `SEARCH BREADTH FIRST BY <col> SET __djogi_search_seq` /
/// `SEARCH DEPTH FIRST BY <col> SET __djogi_search_seq` on the
/// recursive CTE and auto-prepend `ORDER BY __djogi_search_seq` on
/// the outer SELECT so callers see BFS / DFS order without an
/// explicit `order_by` call.
///
/// `Default` skips the SEARCH clause entirely and lets Postgres
/// pick — typically a depth-first walk per recursion step but
/// without the synthetic sequence column. `bfs` produces clean
/// top-down generation bands by elephant birth year; `dfs` walks
/// one matriline chain at a time, useful when reading lineage as
/// "follow this elephant's lineage all the way back."
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Order {
    /// No SEARCH clause; Postgres-default traversal.
    Default,
    /// `SEARCH BREADTH FIRST BY estimated_birth_year`.
    Bfs,
    /// `SEARCH DEPTH FIRST BY estimated_birth_year`.
    Dfs,
}

#[derive(Serialize, Clone)]
struct LineageRow {
    depth: i32,
    id: String,
    name: String,
    mother_id: Option<String>,
    mother_name: Option<String>,
    birth_year: Option<i16>,
    sex: Option<String>,
}

pub async fn run(
    ctx: &mut DjogiContext,
    matriarch: &str,
    max_depth: i32,
    typed: bool,
    order: Order,
    format: Format,
    out: Option<&Path>,
) -> Result<()> {
    if typed {
        return run_typed(ctx, matriarch, max_depth, order, format, out).await;
    }
    // Recursive CTE walks matrilineal descendants level by level —
    // the demo follows mother_id only (single-edge walk) which mirrors
    // herd-society semantics: matrilines are the social unit, fathers
    // are peripheral. The `depth` bound stays inside the recursive arm
    // so Postgres prunes early rather than forming the entire tree
    // before filtering.
    const SQL: &str = "WITH RECURSIVE descent AS (
            SELECT
                e.id,
                e.name,
                e.mother_id,
                e.estimated_birth_year,
                e.tags,
                0 AS depth
            FROM elephants e
            WHERE e.name = $1 AND e.mother_id IS NULL
            UNION ALL
            SELECT
                e.id,
                e.name,
                e.mother_id,
                e.estimated_birth_year,
                e.tags,
                d.depth + 1
            FROM elephants e
            JOIN descent d ON e.mother_id = d.id
            WHERE d.depth + 1 <= $2
        )
        SELECT
            d.depth          AS depth,
            d.id             AS id,
            d.name           AS name,
            d.mother_id      AS mother_id,
            p.name           AS mother_name,
            d.estimated_birth_year AS birth_year,
            d.tags->>'sex'   AS sex
        FROM descent d
        LEFT JOIN elephants p ON p.id = d.mother_id
        ORDER BY d.depth, d.name";
    let binds: &[&(dyn ToSql + Sync)] = &[&matriarch, &max_depth];
    let rows = ctx.raw_rows(SQL, binds).await?;

    let lineage: Vec<LineageRow> = rows
        .iter()
        .map(|row| LineageRow {
            depth: row.get::<_, i32>("depth"),
            id: row.get::<_, i64>("id").to_string(),
            name: row.get::<_, String>("name"),
            mother_id: row
                .get::<_, Option<i64>>("mother_id")
                .map(|v| v.to_string()),
            mother_name: row.get::<_, Option<String>>("mother_name"),
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
        if let Some(mid) = &r.mother_id {
            let mid_node = output::mermaid_node_id(mid.parse::<i64>().unwrap_or(0));
            output::write_line(target, &format!("    {mid_node} --> {id}"))?;
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

    output::write_line(target, "| Depth | Name | Mother | Birth year | Sex |")?;
    output::write_line(target, "|---:|---|---|---:|---|")?;
    for r in rows {
        let mother = r.mother_name.as_deref().unwrap_or("—");
        let birth = r
            .birth_year
            .map(|y| y.to_string())
            .unwrap_or_else(|| "—".to_string());
        let sex = r.sex.as_deref().unwrap_or("—");
        output::write_line(
            target,
            &format!(
                "| {} | {} | {} | {} | {} |",
                r.depth, r.name, mother, birth, sex
            ),
        )?;
    }
    Ok(())
}

/// Typed-builder lineage walk — exercises Phase 8-Zero Cluster B's
/// `tree_descendants(edge, root_id)` + `search_breadth_first_by` /
/// `search_depth_first_by` end-to-end. Same matrilineal direction as
/// the raw-SQL path above: walks `mother_id` only (single-edge),
/// rendering shows generation bands (BFS) or matriline chains (DFS).
async fn run_typed(
    ctx: &mut DjogiContext,
    matriarch: &str,
    max_depth: i32,
    order: Order,
    format: Format,
    out: Option<&Path>,
) -> Result<()> {
    // Resolve the matriarch by name first — the typed
    // `tree_descendants` builder takes a `HeerId` root, not a name.
    // We look up via `raw_query` because `Elephant.name` is
    // `Tracked<String>`, which doesn't impl `IntoFilterValue` today
    // (Tracked is a write-side wrapper for change-tracking; the
    // filter surface treats the underlying type — but the macro
    // emits the field type verbatim in `ElephantFields::name()`,
    // surfacing as `FieldRef<Elephant, Tracked<String>>`). Switching
    // to a typed filter call here would require either an
    // `IntoFilterValue for Tracked<T>` impl in the framework or a
    // model-side rename to plain `String`. Both are out of scope
    // for the typed-lineage demo; the raw lookup is one row + one
    // bind, no semantic loss.
    let matriarch_id: i64 = ctx
        .raw_scalar(
            "SELECT id FROM elephants WHERE name = $1 AND mother_id IS NULL LIMIT 1",
            &[&matriarch],
        )
        .await
        .context("matriarch lookup failed")?;
    let matriarch_id =
        djogi::HeerId::from_i64(matriarch_id).context("matriarch id is not a valid HeerId")?;

    // The depth cap is bound as a u32 by the framework's
    // `with_max_depth(u32)` (Phase 8-Zero Cluster B post-fixup —
    // bound as i32 against int4 internally). Clamp negative
    // user-supplied values to zero to match the contract.
    let depth_cap: u32 = max_depth.max(0) as u32;

    // `Elephant` declares two self-FKs (`mother_id`, `father_id`),
    // so the inherent `Model::tree_descendants` / `tree_ancestors`
    // sugars require `#[model(tree_edge = "...")]` to disambiguate.
    // The matrilineal-lineage demo always walks `mother_id` and
    // we don't want to bias the model toward one edge for the
    // mating-pairs demo's sake — so we pass the explicit edge via
    // the `QuerySet::tree_descendants(edge, root_id)` form, which
    // takes a typed `RelationPath<Elephant, Elephant>` from
    // `ElephantRelated::mother()`. The macro generates that
    // accessor automatically from the `mother_id` field by
    // stripping the `_id` suffix.
    let qs = Elephant::objects()
        .tree_descendants(ElephantRelated::mother(), matriarch_id)
        .with_max_depth(depth_cap);

    let walked: Vec<(Elephant, i32, Vec<String>)> = match order {
        Order::Default => qs.fetch_all_with_paths(ctx).await?,
        Order::Bfs => {
            qs.search_breadth_first_by(ElephantFields.estimated_birth_year())
                .fetch_all_with_paths(ctx)
                .await?
        }
        Order::Dfs => {
            qs.search_depth_first_by(ElephantFields.estimated_birth_year())
                .fetch_all_with_paths(ctx)
                .await?
        }
    };

    // Re-fetch parents in one round trip so the markdown / mermaid
    // output can show "Mother: <name>" — the recursive walk only
    // returns each elephant's full row, not its mother's row, and
    // joining mother names into the recursive CTE projection isn't
    // available in the typed builder today (would require a
    // post-fetch eager-load surface for self-FK chains, which is
    // outside Phase 8-Zero scope).
    let mother_ids: Vec<djogi::HeerId> = walked
        .iter()
        .filter_map(|(e, _, _)| e.mother_id.as_ref().map(|fk| fk.key()))
        .collect();
    let mothers_by_id: std::collections::HashMap<djogi::HeerId, String> = if mother_ids.is_empty() {
        std::collections::HashMap::new()
    } else {
        let bind_ids: Vec<i64> = mother_ids.iter().map(|h| h.as_i64()).collect();
        let rows = ctx
            .raw_rows(
                "SELECT id, name FROM elephants WHERE id = ANY($1::bigint[])",
                &[&bind_ids],
            )
            .await?;
        rows.iter()
            .map(|r| {
                (
                    djogi::HeerId::from_i64(r.get::<_, i64>("id")).expect("valid HeerId"),
                    r.get::<_, String>("name"),
                )
            })
            .collect()
    };

    let lineage: Vec<LineageRow> = walked
        .into_iter()
        .map(|(e, depth, _path)| {
            let mother_id_str = e.mother_id.as_ref().map(|fk| fk.key().as_i64().to_string());
            let mother_name = e
                .mother_id
                .as_ref()
                .and_then(|fk| mothers_by_id.get(&fk.key()).cloned());
            let sex = e.tags.data.sex.clone();
            LineageRow {
                depth,
                id: e.id.as_i64().to_string(),
                name: e.name.clone().into_inner(),
                mother_id: mother_id_str,
                mother_name,
                birth_year: e.estimated_birth_year,
                sex,
            }
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
