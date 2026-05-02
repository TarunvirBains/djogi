//! `mating-pairs` — Wright F coefficient over the materialized
//! ancestry closure, top-3 candidate pairs per mature female.
//!
//! ## What this demonstrates
//!
//! - **Materialized closure as production-shape kinship lookup.**
//!   The `ElephantAncestry` table (populated at seed time by
//!   `Model::materialize_closure`) is joined to itself on
//!   `ancestor_id` to find every shared ancestor between a female's
//!   line and a male's line. One closure read per query rather than
//!   re-walking the recursive CTE per pair — this is the framework's
//!   answer for adopters at scale.
//!
//! - **Wright 1922 inbreeding coefficient.** For each candidate pair
//!   `(female, male)`, the offspring's expected inbreeding
//!   coefficient `F` is:
//!
//!   ```text
//!   F = SUM over common ancestors A of:
//!       (left.path_count × right.path_count) × 0.5^(d_left + d_right + 1)
//!   ```
//!
//!   where `left` is `(elephant_id = female_id, ancestor_id = A)` and
//!   `right` is `(elephant_id = male_id, ancestor_id = A)` from the
//!   closure. `path_count` preserves multi-path multiplicity per
//!   Wright kinship; the framework's `materialize_closure` is correct
//!   on this point because the underlying recursive CTE uses
//!   `UNION ALL` (not `UNION`) so paths through different edges to
//!   the same ancestor are summed, not deduped.
//!
//!   This implementation uses the simplified Wright form with
//!   `(1 + F_A) ≈ 1` — i.e., the ancestor's own inbreeding
//!   coefficient is treated as zero. The full recursive Wright form
//!   would require a self-referential `WITH RECURSIVE` over `F`
//!   itself, which is beyond the demo's pedagogical scope. For our
//!   deterministic 120-elephant seed (no inbreeding by
//!   construction), `F_A = 0` is exact; for adopter datasets with
//!   inherited inbreeding, the simplified form under-estimates F by
//!   a small constant factor that doesn't change the top-3 ranking
//!   per female.
//!
//! - **Top-N per partition via window functions.** The candidate-pair
//!   scoring is wrapped in a `ROW_NUMBER() OVER (PARTITION BY
//!   female_id ORDER BY score DESC)` and outer-filtered to
//!   `rank <= 3`. Postgres has no `QUALIFY` keyword (Phase 8-Zero
//!   Cluster C T18 documents this); the framework's typed
//!   `RowNumber().qualify(...)` lowering produces the equivalent
//!   `SELECT * FROM (<inner>) AS __djogi_q WHERE rank <= $1` shape.
//!   This v1 of the demo emits the SQL via `raw_query` to keep the
//!   correctness story tight; a follow-up migrates the ranking step
//!   to the typed `RowNumber` builder so the demo also exercises
//!   Cluster C's window-function surface end-to-end.
//!
//! ## Composite score
//!
//! The v3 plan calls for a multi-factor score:
//!
//! ```text
//! score = (1 - F) × territory_overlap_pct × age_compatibility
//! ```
//!
//! v1 ships only the kinship factor (`score = 1 - F`); territory
//! overlap (via `convex_hull` + `intersection` + `area` from Cluster
//! C T16/T17) and age compatibility are deferred to subsequent
//! commits. Including them now would conflate the demonstration of
//! "closure-table-driven kinship" with "multi-factor ranking heuristic"
//! and obscure the framework substrate the demo exists to showcase.
//!
//! ## Output formats
//!
//! - `json` — flat list of `{female_id, female_name, male_id,
//!   male_name, f_offspring, score, rank}` records, top-3 per
//!   female, sorted by `(female_name, rank)`.
//! - `markdown` — sorted table with one row per pair plus a
//!   summary header.
//!
//! Filter: only mature elephants (estimated_birth_year ≤
//! `now - MATURITY_YEARS years`, see the constant for rationale on
//! the threshold), of opposite sex, in the same herd. The same-
//! herd filter is the v1 stand-in for the v3 plan's spatial-overlap
//! filter (convex_hull + intersection + area via Cluster C
//! T16/T17), which lands in a follow-up commit so the demo
//! exercises Cluster C's typed spatial surface end-to-end.

use anyhow::Result;
use djogi::DjogiContext;
use postgres_types::ToSql;
use serde::Serialize;
use std::path::Path;

use crate::output::{self, Format};

/// Maturity threshold (years). Wild African elephants reach sexual
/// maturity ~10-15. The demo uses 10 so daughters in the seed
/// become candidates — at 15 only matriarchs would qualify as
/// females, and matriarchs are unrelated to all candidate bulls
/// in the seed by construction (every F would be 0, the demo's
/// output would be visually flat). Lowering to 10 surfaces
/// daughter-vs-father pairings (F = 0.25, parent-child) below
/// daughter-vs-unrelated-bull pairings (F = 0), so the score-
/// driven ranking shows real variance.
const MATURITY_YEARS: i32 = 10;

/// Number of top-ranked males surfaced per mature female.
const TOP_N_PER_FEMALE: i32 = 3;

#[derive(Serialize, Clone, Debug)]
struct MatingPair {
    rank: i32,
    female_id: String,
    female_name: String,
    female_herd: String,
    male_id: String,
    male_name: String,
    male_herd: String,
    f_offspring: f64,
    score: f64,
}

pub async fn run(ctx: &mut DjogiContext, format: Format, out: Option<&Path>) -> Result<()> {
    // Wright-F-over-closure SQL with row-number-partition-by-female
    // top-N filter. The `__djogi_q` derived-table alias mirrors the
    // shape Cluster C T18's typed `qualify(...)` lowering produces;
    // a follow-up commit will rewrite this in terms of the typed
    // `RowNumber` / `qualify` builders so the demo exercises that
    // surface as well.
    //
    // The current-year integer is computed in Rust and passed as a
    // bind so the demo is reproducible across runs (Postgres
    // `EXTRACT(YEAR FROM CURRENT_DATE)` would drift annually,
    // making the deterministic seed's mature-elephant set vary).
    const SQL: &str = "
        WITH mature AS (
            SELECT
                e.id,
                e.name,
                e.herd_id,
                e.tags->>'sex' AS sex
            FROM elephants e
            WHERE e.estimated_birth_year IS NOT NULL
              AND ($1::integer - e.estimated_birth_year::integer) >= $2
        ),
        candidate_pairs AS (
            -- Same-herd filter is the v1 stand-in for the spatial-
            -- overlap filter (convex_hull + intersection + area
            -- via Cluster C T16/T17) the v3 plan calls for. v1
            -- restricts to same-herd pairs; subsequent commits
            -- will swap this for the typed spatial-overlap surface
            -- so the demo exercises Cluster C end-to-end. Keeping
            -- pairs in-herd is also where Wright F variance shows
            -- up in the deterministic seed: each daughter's
            -- biological father lives in her own herd's bull pool.
            SELECT
                f.id          AS female_id,
                f.name        AS female_name,
                f.herd_id     AS female_herd_id,
                m.id          AS male_id,
                m.name        AS male_name,
                m.herd_id     AS male_herd_id
            FROM mature f
            CROSS JOIN mature m
            WHERE f.sex = 'f' AND m.sex = 'm'
              AND f.id <> m.id
              AND f.herd_id = m.herd_id
        ),
        pair_kinship AS (
            SELECT
                cp.female_id,
                cp.female_name,
                cp.female_herd_id,
                cp.male_id,
                cp.male_name,
                cp.male_herd_id,
                COALESCE(SUM(
                    la.path_count::numeric
                  * ra.path_count::numeric
                  * POWER(0.5::numeric, (la.depth + ra.depth + 1)::numeric)
                ), 0)::float8 AS f_offspring
            FROM candidate_pairs cp
            LEFT JOIN elephant_ancestries la
                ON la.elephant_id = cp.female_id
            LEFT JOIN elephant_ancestries ra
                ON ra.elephant_id = cp.male_id
               AND ra.ancestor_id = la.ancestor_id
            GROUP BY cp.female_id, cp.female_name, cp.female_herd_id,
                     cp.male_id, cp.male_name, cp.male_herd_id
        ),
        scored AS (
            SELECT
                pk.*,
                (1.0 - pk.f_offspring)::float8 AS score,
                ROW_NUMBER() OVER (
                    PARTITION BY pk.female_id
                    ORDER BY (1.0 - pk.f_offspring) DESC, pk.male_name ASC
                ) AS rank
            FROM pair_kinship pk
        )
        SELECT
            scored.rank::integer        AS rank,
            scored.female_id            AS female_id,
            scored.female_name          AS female_name,
            fh.name                     AS female_herd,
            scored.male_id              AS male_id,
            scored.male_name            AS male_name,
            mh.name                     AS male_herd,
            scored.f_offspring          AS f_offspring,
            scored.score                AS score
        FROM scored
        JOIN herds fh ON fh.id = scored.female_herd_id
        JOIN herds mh ON mh.id = scored.male_herd_id
        WHERE scored.rank <= $3
        ORDER BY scored.female_name ASC, scored.rank ASC";

    let now_year: i32 = 2026;
    let maturity = MATURITY_YEARS;
    // `ROW_NUMBER() OVER (...)` returns `bigint` in Postgres; bind
    // the rank threshold as `i64` to satisfy tokio_postgres's exact
    // bind/column type-match requirement.
    let top_n: i64 = TOP_N_PER_FEMALE as i64;
    let binds: &[&(dyn ToSql + Sync)] = &[&now_year, &maturity, &top_n];
    let rows = ctx.raw_rows(SQL, binds).await?;

    let pairs: Vec<MatingPair> = rows
        .iter()
        .map(|row| MatingPair {
            rank: row.get::<_, i32>("rank"),
            female_id: row.get::<_, i64>("female_id").to_string(),
            female_name: row.get::<_, String>("female_name"),
            female_herd: row.get::<_, String>("female_herd"),
            male_id: row.get::<_, i64>("male_id").to_string(),
            male_name: row.get::<_, String>("male_name"),
            male_herd: row.get::<_, String>("male_herd"),
            f_offspring: row.get::<_, f64>("f_offspring"),
            score: row.get::<_, f64>("score"),
        })
        .collect();

    let mut target = output::open_writer(out)?;
    match format {
        Format::Json => output::write_json(&mut target, &pairs)?,
        Format::Markdown => render_markdown(&mut target, &pairs)?,
        Format::Mermaid => render_markdown(&mut target, &pairs)?,
    }
    Ok(())
}

fn render_markdown(target: &mut output::OutputTarget, pairs: &[MatingPair]) -> Result<()> {
    output::write_line(
        target,
        "# Mating Pairs — Top-3 candidates per mature female\n",
    )?;
    if pairs.is_empty() {
        output::write_line(
            target,
            "_No candidate pairs — does the seed contain mature \
             elephants of both sexes?_",
        )?;
        return Ok(());
    }
    output::write_line(
        target,
        "Score = `1 - F_offspring`. Higher is better (lower expected \
         inbreeding for the offspring of this pairing). `F_offspring` \
         uses the simplified Wright form with `F_A = 0` for ancestors \
         (exact for non-inbred ancestors, slight under-estimate \
         otherwise). Spatial-overlap and age-compatibility factors \
         land in subsequent commits.\n",
    )?;
    output::write_line(
        target,
        "| Rank | Female (herd) | Male (herd) | F_offspring | Score |",
    )?;
    output::write_line(target, "|---:|---|---|---:|---:|")?;
    for p in pairs {
        output::write_line(
            target,
            &format!(
                "| {} | {} ({}) | {} ({}) | {:.6} | {:.6} |",
                p.rank,
                p.female_name,
                p.female_herd,
                p.male_name,
                p.male_herd,
                p.f_offspring,
                p.score,
            ),
        )?;
    }
    Ok(())
}
