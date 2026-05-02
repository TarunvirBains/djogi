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
//!   coefficient is treated as zero. The full recursive Wright
//!   form would require a self-referential `WITH RECURSIVE` over
//!   `F` itself, which is beyond the demo's pedagogical scope.
//!
//!   **Caution for adopter use:** for our deterministic 120-elephant
//!   seed (matriarchs and bulls are unrelated by construction so
//!   ancestors carry no inbreeding) `F_A = 0` is exact. For real-
//!   world adopter datasets with inherited inbreeding, each affected
//!   term is under-counted by `(1 + F_A)`, and `(1 + F_A)` is not
//!   necessarily small — populations with deep linebreeding can
//!   carry F_A values that shift the top-N ranking. Any production
//!   adopter computing kinship in earnest should extend this query
//!   to the full Wright recurrence, or substitute a heavier-weight
//!   library such as a published Wright/Malécot implementation.
//!   The demo's value here is showing the framework substrate (one
//!   `materialize_closure` call + indexed self-join), not shipping
//!   a kinship library.
//!
//! - **Top-N per partition via window functions.** The candidate-pair
//!   scoring is wrapped in a `ROW_NUMBER() OVER (PARTITION BY
//!   female_id ORDER BY score DESC)` and outer-filtered to
//!   `rank <= 3`. Postgres has no `QUALIFY` keyword (Phase 8-Zero
//!   Cluster C T18 documents this); the framework's typed
//!   `RowNumber().qualify(...)` lowering produces the equivalent
//!   `SELECT * FROM (<inner>) AS __djogi_q WHERE rank <= $1` shape.
//!
//!   **Why this demo emits via `ctx.raw_rows` instead of the typed
//!   builder:** Cluster C's typed `RowNumber().qualify(...)`
//!   surface is bolted to `AnnotatedQuerySet<T, A>` over a single
//!   `Model T`. The mating-pairs ranking surface is a multi-CTE
//!   pair shape (`(female, male)` rows derived from a
//!   `WITH mature → candidate_pairs → pair_kinship` chain over
//!   `elephant_ancestries` self-joined) — there is no single
//!   `T = Model` that fits. The emitted SQL nonetheless mirrors the
//!   exact shape the typed builder produces (same `__djogi_q`
//!   alias, same outer-WHERE), so adopters reading this demo
//!   alongside Cluster C's typed-surface tests see end-to-end
//!   continuity. Bridging the typed surface to multi-CTE shapes
//!   is tracked as **issue #84** for follow-up framework work.
//!
//! ## Composite score
//!
//! The v3 plan calls for a multi-factor score:
//!
//! ```text
//! score = (1 - F) × territory_overlap_pct × age_compatibility
//! ```
//!
//! T24c lands two of three factors: kinship `(1 - F)` and
//! `territory_overlap_pct`. The latter is computed via per-herd
//! convex hulls over recorded sightings + `ST_Area(ST_Intersection
//! (a, b))/ST_Area(a)` — the same SQL primitives Cluster C T16/T17
//! ships through the typed `FieldRef::convex_hull()` aggregate and
//! `Expr::area_of_intersection` / `Expr::area_of` scalar surface.
//! The demo emits the SQL inline because the typed surface bolts
//! `convex_hull` to `AnnotatedQuerySet<T, A>` over a single Model
//! while the mating-pairs query is a multi-CTE pair shape (same
//! reasoning as T24b — see #84 for the framework gap).
//!
//! `age_compatibility` is the third factor and is deferred to a
//! follow-up commit (T24d) — the seed's age distribution is
//! deterministic and clustered into 3 birth-year cohorts (matriarch
//! 2010, daughter 2016, grandkid 2022, plus 2008-2010 bulls), so
//! age compatibility computed naively (e.g., `1 - |Δyears|/30`)
//! produces near-binary output. Surfacing it requires either richer
//! seed data or a more nuanced compatibility curve, which is a
//! polish-pass concern.
//!
//! ## Output formats
//!
//! - `json` — flat list of `{rank, female_id, female_name,
//!   female_herd, male_id, male_name, male_herd, f_offspring,
//!   territory_overlap_pct, score}` records, top-3 per female,
//!   sorted by `(female_name, rank)`.
//! - `markdown` — sorted table with one row per pair plus a
//!   summary header.
//! - `mermaid` — `graph LR` with one node per participating elephant
//!   (label includes herd) and one directed `female --> male` edge
//!   per pair (label `#rank score=N.NNN`).
//!
//! Filter: only mature elephants (estimated_birth_year ≤
//! `now - MATURITY_YEARS years`, see the constant for rationale on
//! the threshold), of opposite sex, whose herd-territory polygons
//! spatially overlap. The territory polygon is the convex hull of
//! every recorded `Sighting.location` belonging to a herd member;
//! pairs whose hulls don't intersect (`territory_overlap_pct = 0`)
//! are filtered out before kinship computation. In the
//! deterministic seed, herd centers are far apart relative to the
//! per-herd sighting jitter, so `overlap = 1.0` for same-herd
//! pairs and `overlap = 0` for cross-herd pairs. Adopters who seed
//! cross-territory wandering elephants (sightings outside the home
//! range) will see fractional overlaps surface; the demo's filter
//! and the composite-score multiplication are both ready for that
//! data without further code changes.

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
    /// Fraction in `[0, 1]` of the female's herd-territory polygon
    /// that overlaps the male's herd-territory polygon. Computed
    /// from the convex hull of each herd's recorded sightings (a
    /// rough but operator-meaningful proxy for "where the herd
    /// actually ranges"). 1.0 is the same-herd self-overlap case.
    territory_overlap_pct: f64,
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
        herd_hulls AS (
            -- Per-herd convex-hull aggregate over recorded sightings.
            -- `ST_Collect` rolls all `Sighting.location` GeoPoints
            -- into a single MultiPoint per herd, then `ST_ConvexHull`
            -- produces the smallest enclosing polygon. The outer
            -- `::geography` cast preserves the meters-units invariant
            -- the rest of the framework's spatial surface uses
            -- (Cluster C T17). Mirrors the SQL the framework's typed
            -- `FieldRef::convex_hull()` aggregate emits — but expressed
            -- inline here because the typed aggregate surface is
            -- bolted to `AnnotatedQuerySet<T, A>` over a single Model
            -- (see #84), and the mating-pairs query is a multi-CTE
            -- pair shape that doesn't fit `T = Model`.
            --
            -- **Precondition.** Herds with zero recorded sightings
            -- are absent from this CTE; the inner JOIN below means
            -- mature elephants from such herds disappear from
            -- candidate_pairs. The framework seeds 50
            -- sightings/herd, so this is never observed in the
            -- example, but adopters running the demo against a
            -- subset of herds that lack sightings will see no
            -- candidates surface for those herds. The empty-output
            -- diagnostic flags this case explicitly.
            SELECT
                s.elephant_id_to_herd                     AS herd_id,
                ST_ConvexHull(ST_Collect(s.location::geometry))::geography AS hull
            FROM (
                SELECT e.herd_id AS elephant_id_to_herd, s.location
                FROM sightings s
                JOIN elephants e ON e.id = s.elephant_id
            ) s
            GROUP BY s.elephant_id_to_herd
        ),
        candidate_pairs AS (
            -- Cross-product of mature (female, male) pairs filtered to
            -- non-zero spatial overlap between their herd territories.
            -- The same-herd self-overlap case (`fh.hull = mh.hull`)
            -- produces overlap_pct = 1.0; cross-herd pairs whose hulls
            -- don't intersect produce 0 — which the kinship-scored
            -- composite would multiply out anyway, but filtering at
            -- this stage keeps the closure self-join in `pair_kinship`
            -- bounded to spatially-plausible candidates.
            --
            -- Mirrors the framework's `Expr::area_of_intersection` /
            -- `Expr::area_of` SpatialExpr surface (Cluster C T16/T17)
            -- inline here for the same multi-CTE reason as above.
            SELECT
                f.id          AS female_id,
                f.name        AS female_name,
                f.herd_id     AS female_herd_id,
                m.id          AS male_id,
                m.name        AS male_name,
                m.herd_id     AS male_herd_id,
                CASE
                    WHEN ST_Area(fh.hull) = 0 THEN 0::float8
                    -- Same-herd self-overlap is mathematically 1.0 but
                    -- the geometry-vs-geography overlay can drift in
                    -- floating point. Short-circuit explicitly so the
                    -- JSON surface guarantees exact 1.0 for adopters.
                    WHEN f.herd_id = m.herd_id THEN 1.0::float8
                    -- LEAST/GREATEST clamps the ratio to [0, 1] in
                    -- case the geometry/geography overlay rounds the
                    -- intersection slightly larger than the source
                    -- hull (within numeric tolerance). Mathematically
                    -- impossible for exact inputs; in practice
                    -- defensive against PostGIS overlay tolerances on
                    -- highly-clustered point sets.
                    ELSE LEAST(1.0::float8, GREATEST(0.0::float8, (
                        ST_Area(
                            ST_Intersection(fh.hull::geometry, mh.hull::geometry)::geography
                        ) / ST_Area(fh.hull)
                    )::float8))
                END           AS territory_overlap_pct
            FROM mature f
            CROSS JOIN mature m
            JOIN herd_hulls fh ON fh.herd_id = f.herd_id
            JOIN herd_hulls mh ON mh.herd_id = m.herd_id
            WHERE f.sex = 'f' AND m.sex = 'm'
              AND f.id <> m.id
        ),
        pair_kinship AS (
            SELECT
                cp.female_id,
                cp.female_name,
                cp.female_herd_id,
                cp.male_id,
                cp.male_name,
                cp.male_herd_id,
                cp.territory_overlap_pct,
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
            WHERE cp.territory_overlap_pct > 0
            GROUP BY cp.female_id, cp.female_name, cp.female_herd_id,
                     cp.male_id, cp.male_name, cp.male_herd_id,
                     cp.territory_overlap_pct
        ),
        scored AS (
            SELECT
                pk.*,
                ((1.0 - pk.f_offspring) * pk.territory_overlap_pct)::float8 AS score,
                ROW_NUMBER() OVER (
                    PARTITION BY pk.female_id
                    ORDER BY ((1.0 - pk.f_offspring) * pk.territory_overlap_pct) DESC,
                             pk.male_name ASC
                ) AS rank
            FROM pair_kinship pk
        )
        SELECT
            scored.rank::integer            AS rank,
            scored.female_id                AS female_id,
            scored.female_name              AS female_name,
            fh.name                         AS female_herd,
            scored.male_id                  AS male_id,
            scored.male_name                AS male_name,
            mh.name                         AS male_herd,
            scored.f_offspring              AS f_offspring,
            scored.territory_overlap_pct    AS territory_overlap_pct,
            scored.score                    AS score
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
            territory_overlap_pct: row.get::<_, f64>("territory_overlap_pct"),
            score: row.get::<_, f64>("score"),
        })
        .collect();

    let mut target = output::open_writer(out)?;
    match format {
        Format::Json => output::write_json(&mut target, &pairs)?,
        Format::Markdown => render_markdown(&mut target, &pairs)?,
        Format::Mermaid => render_mermaid(&mut target, &pairs)?,
    }
    Ok(())
}

/// `graph LR` of the top-N pairs per female. One node per
/// participating elephant (herd-prefixed label so visually distinct
/// herds stay readable in dense graphs); one directed edge per pair
/// drawn `female --> male` with the score on the edge label.
fn render_mermaid(target: &mut output::OutputTarget, pairs: &[MatingPair]) -> Result<()> {
    output::write_line(target, "graph LR")?;
    if pairs.is_empty() {
        output::write_line(
            target,
            "    n0[\"No candidate pairs — verify seed has mature \
             elephants of both sexes with sex tags populated\"]",
        )?;
        return Ok(());
    }

    // Each elephant in the result set becomes a single node. Use
    // mermaid_node_id (which encodes the i64 id) so two distinct
    // elephants can never collide on a node label.
    let mut nodes: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for p in pairs {
        let f_node = output::mermaid_node_id(p.female_id.parse::<i64>().unwrap_or(0));
        let m_node = output::mermaid_node_id(p.male_id.parse::<i64>().unwrap_or(0));
        nodes.entry(f_node).or_insert_with(|| {
            output::escape_label(&format!("{} ({})", p.female_name, p.female_herd))
        });
        nodes
            .entry(m_node)
            .or_insert_with(|| output::escape_label(&format!("{} ({})", p.male_name, p.male_herd)));
    }
    for (id, label) in &nodes {
        output::write_line(target, &format!("    {id}[\"{label}\"]"))?;
    }
    for p in pairs {
        let f_node = output::mermaid_node_id(p.female_id.parse::<i64>().unwrap_or(0));
        let m_node = output::mermaid_node_id(p.male_id.parse::<i64>().unwrap_or(0));
        output::write_line(
            target,
            &format!(
                "    {f_node} -->|\"#{} score={:.3}\"| {m_node}",
                p.rank, p.score
            ),
        )?;
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
            "_No candidate pairs — verify the seed has mature \
             elephants of both sexes (`tags->>'sex' = 'f'` / `'m'` \
             populated, `estimated_birth_year` set), every \
             candidate's `herd_id` corresponds to a herd with \
             recorded sightings (the `herd_hulls` CTE drops herds \
             with zero sightings), and at least two such elephants \
             share spatially-overlapping herd-territory polygons. \
             The most common cause is running `migrate` from a \
             pre-T22 snapshot whose elephants don't have sex tags; \
             the second is a partial seed where some herds have \
             no sightings yet._",
        )?;
        return Ok(());
    }
    output::write_line(
        target,
        "Score = `1 - F_offspring`. Higher is better (lower expected \
         inbreeding for the offspring of this pairing). `F_offspring` \
         uses the simplified Wright form with `F_A = 0` for ancestors \
         — **exact** for non-inbred ancestors, but **production \
         adopters with linebreeding should use the full Wright \
         recurrence**: each affected term is under-counted by \
         `(1 + F_A)` and that factor can be substantial enough to \
         shift the top-N ranking. The demo's role is showcasing the \
         framework substrate (`materialize_closure` + indexed \
         self-join), not shipping a kinship library. \
         `territory_overlap_pct` is the fraction of the female's \
         herd-territory polygon (convex hull of recorded sightings) \
         covered by the male's herd-territory polygon — 1.0 for the \
         same-herd case, lower for spatially-disjoint cross-herd \
         pairs (filtered out before kinship computation when \
         `overlap = 0`). Composite `score = (1 - F) × overlap`. \
         Age-compatibility multiplier lands in a follow-up commit.\n",
    )?;
    output::write_line(
        target,
        "| Rank | Female (herd) | Male (herd) | F_offspring | Overlap | Score |",
    )?;
    output::write_line(target, "|---:|---|---|---:|---:|---:|")?;
    for p in pairs {
        output::write_line(
            target,
            &format!(
                "| {} | {} ({}) | {} ({}) | {:.6} | {:.4} | {:.6} |",
                p.rank,
                p.female_name,
                p.female_herd,
                p.male_name,
                p.male_herd,
                p.f_offspring,
                p.territory_overlap_pct,
                p.score,
            ),
        )?;
    }
    Ok(())
}
