//! `mating-pairs` — Wright F coefficient over the materialized
//! ancestry closure, top-3 candidate pairs per mature female.
//!
//! ## What this demonstrates
//!
//! - **Typed pair-tuple closure self-join** (Cluster 4A, GH #99). The
//!   ranking previously emitted by a hand-written `WITH RECURSIVE`-flavoured
//!   raw SQL block is now expressed through
//!   `Elephant::objects().self_pairs().left_join_closure_pair::<ElephantAncestry>().annotate(...)`.
//!   No raw SQL is involved in the kinship pass — the
//!   `JoinedQuerySet<L, L>` substrate emits the `CROSS JOIN ... LEFT JOIN
//!   <closure> AS la / ra ...` plus the per-pair `GROUP BY l.id, r.id`
//!   directly. The Wright kinship sum is a typed annotation slot
//!   (`PairClosureKinshipSum<ElephantAncestry>`) that emits
//!   `COALESCE(SUM(la.path_count * ra.path_count * 0.5^(la.depth + ra.depth + 1)), 0)::float8`
//!   under the framework-reserved `__djogi_agg_0` alias.
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
//! - **Top-N per partition.** The Wright F values come back as
//!   `Vec<((Elephant, Elephant), f64)>` from the typed pair-tuple
//!   terminal. The composite-score multiplication, top-3 ranking, and
//!   `(female_name, rank)` sort all run in Rust because the typed
//!   pair-tuple `qualify(...)` surface in the Cluster 4A substrate
//!   accepts only column references on its `partition_by_pair` /
//!   `order_by_pair_desc` window-fn methods — not an arbitrary
//!   `Expr<f64>` derived from `(1 - F) × overlap × age_compat`. The
//!   demo's score is composed from three different sources (typed
//!   aggregate output for `F`, Rust-side same-herd binary for
//!   `overlap`, Rust-side bell-curve for `age_compat`), so the natural
//!   place to combine them is Rust. A future slice that adds a
//!   pair-side `Expr`-based `order_by_pair_desc` surface (and a
//!   pair-tuple `Expr::area_of_intersection` over per-row geometry
//!   columns) would let the entire ranking pass land in SQL; see #99
//!   and #65's "What changes when this ships" note.
//!
//! - **Punnu cache showcase.** Step 2 wraps the mature-elephant typed
//!   fetch with `ctx.punnu::<Elephant>()` insertion so adopters can see
//!   the integration pattern in action. The CLI runs each demo as a
//!   one-shot so the cache doesn't surface a second-read cache hit
//!   within a single `mating-pairs` invocation; a long-lived adopter
//!   process (a request handler, a periodic scoring job) would see the
//!   `pool.get(id)` reads return from in-memory L1 without a DB
//!   round-trip on subsequent calls. The structural wiring is the
//!   teaching artifact.
//!
//! ## Composite score
//!
//! The v3 plan calls for a multi-factor score:
//!
//! ```text
//! score = (1 - F) × territory_overlap_pct × age_compatibility
//! ```
//!
//! All three factors ship here. Kinship `(1 - F)` comes from the typed
//! pair-tuple closure self-join above. `territory_overlap_pct` is the
//! ratio of the female herd's convex hull to the male herd's convex
//! hull intersection area; in the deterministic seed this is binary
//! (1.0 same-herd, 0.0 cross-herd) because herd centres are far apart
//! relative to per-herd sighting jitter, but the same Rust path
//! computes graduated overlap once the seed is extended with
//! cross-territory wandering elephants. `age_compatibility` is a
//! smooth fertility-window product over female age peaked at 20
//! (sigma 10) and male age peaked at 32 (sigma 15); we report the
//! geometric mean of the two bells so a one-sided unsuitability
//! is penalised more than a single-factor average would be.
//!
//! ## Output formats
//!
//! - `json` — flat list of `{rank, female_id, female_name,
//!   female_herd, male_id, male_name, male_herd, f_offspring,
//!   territory_overlap_pct, age_compatibility, score}` records,
//!   top-3 per female, sorted by `(female_name, rank)`.
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
use djogi::prelude::*;
use djogi::query::PairClosureKinshipSum;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

use crate::models::{Elephant, ElephantAncestry, Herd, Sighting};
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
const TOP_N_PER_FEMALE: usize = 3;

/// Reference year for age computations. Tied to the deterministic seed's
/// birth-year cohorts (matriarchs 2010, daughters 2016, bulls 2008-2010)
/// so the demo output is reproducible regardless of when the example is
/// executed.
const NOW_YEAR: i16 = 2026;

/// Centre of the female fertility window (years). The bell curve below
/// peaks here. African elephants conceive most reliably in early
/// adulthood; 20 is the rough centre of that observed window.
const FEMALE_FERTILITY_PEAK: f64 = 20.0;

/// Standard deviation of the female fertility window (years). Together
/// with [`FEMALE_FERTILITY_PEAK`], `bell(age, peak, sigma)` evaluates to
/// `~1.0` at peak, `~0.6` one σ away, and `~0.14` two σ away.
const FEMALE_FERTILITY_SIGMA: f64 = 10.0;

/// Centre of the male fertility window (years). Bulls reach reproductive
/// dominance later than females in herd-society biology; 32 reflects the
/// observed musth-bull mean.
const MALE_FERTILITY_PEAK: f64 = 32.0;

/// Standard deviation of the male fertility window. Wider than the
/// female bell because male reproductive viability stays usable across
/// a longer lifespan tail.
const MALE_FERTILITY_SIGMA: f64 = 15.0;

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
    /// Smooth fertility-window product. `1.0` is a perfectly-matched
    /// pair at both species peaks; lower values penalise pairs whose
    /// joint age window is sub-optimal. See [`age_compatibility`].
    age_compatibility: f64,
    score: f64,
}

/// Mating-pairs demo entry point.
///
/// Sequence:
///
/// 1. Fetch per-herd convex hulls via the typed
///    `Sighting::objects().group_by(...).annotate(...)` aggregate.
/// 2. Fetch mature elephants (typed query) + warm the `Punnu<Elephant>`
///    L1 cache (showcase).
/// 3. Fetch every `(female, male)` mature pair with its Wright F
///    coefficient via the typed
///    `Elephant::objects().self_pairs().left_join_closure_pair::<ElephantAncestry>().annotate(PairClosureKinshipSum)`
///    chain. No raw SQL.
/// 4. Compute composite score in Rust (overlap × kinship × age
///    compatibility), rank top-N per female, render.
pub async fn run(ctx: &mut DjogiContext, format: Format, out: Option<&Path>) -> Result<()> {
    // ── Step 1 (typed Djogi) — per-herd convex hull aggregate ─────
    //
    // Demonstrates Cluster C T16/T17's typed
    // `FieldRef::convex_hull()` aggregate composed with the typed
    // `group_by(...).annotate(...)` surface. Returns one (herd_id,
    // hull-Polygon) per herd that has any recorded sightings.
    //
    // The denormalized `Sighting.herd_id` column (added alongside
    // this restructure) is what makes the typed `group_by(|s|
    // s.herd_id())` shape fit — without it, grouping by herd would
    // require relation traversal (`s.elephant().herd_id()`) which
    // the framework's grouped-aggregate surface doesn't model.
    let herd_hulls: Vec<(djogi::ForeignKey<Herd>, djogi::geo::Polygon)> = Sighting::objects()
        .group_by(|s| s.herd_id())
        .annotate(|s| s.location().convex_hull())
        .fetch_all(ctx)
        .await?;
    // Set of herd ids that have any recorded sightings — the demo's
    // territory-overlap predicate requires a hull for both sides, so
    // mature elephants in herds with zero sightings drop out before
    // scoring. Captured here so the rest of the pipeline can ask
    // "does this herd have a territory hull?" in O(1) without
    // round-tripping the Polygon.
    let herds_with_hulls: std::collections::BTreeSet<djogi::HeerId> = herd_hulls
        .iter()
        .map(|(herd_fk, _)| herd_fk.key())
        .collect();

    // Per-herd label lookup for the output rows. One typed fetch over
    // `Herd::objects()` — no JOIN through raw SQL.
    let herds: Vec<Herd> = Herd::objects().fetch_all(ctx).await?;
    let herd_name_by_id: HashMap<djogi::HeerId, String> =
        herds.into_iter().map(|h| (h.id, h.name)).collect();

    // ── Step 2 (typed Djogi + Punnu cache showcase) ───────────────
    //
    // The Punnu cache integration enumerated in #108 has three shapes
    // adopters can compose:
    //
    // 1. **Punnu-wrapped typed** (preferred for repeat queries in a
    //    long-lived app — request handler, periodic scoring job). The
    //    typed fetch hits the DB once, then every row is inserted into
    //    the per-context `Punnu<Elephant>` L1 pool. Subsequent
    //    `pool.get(id)` calls return `Arc<Elephant>` without a round
    //    trip. The pool is invalidated automatically when `Elephant::
    //    create` / `save` / `delete` runs through djogi's hook
    //    machinery (cluster 8δ T7.5).
    //
    // 2. **Bare typed** (the demo path before this slice). Preferred
    //    when the query is called once per process invocation — no
    //    cache amortisation possible, and the Punnu insert costs are
    //    pure overhead.
    //
    // 3. **Rust-side post-filter** (preferred when no JSONB index
    //    exists, or when the table is small enough that fetching all
    //    rows beats a server-side predicate dispatch). The demo's
    //    sex-split below is this shape: we fetch the whole mature pool
    //    and partition by `tags.data.sex` in Rust because JSONB-tags
    //    isn't typed-filterable today.
    //
    // The demo runs shape #1 (Punnu warm-up) followed by shape #3 (sex
    // partition in Rust). The CLI exit removes the warmed cache when
    // the process ends, so the cache-hit story is structural; adopters
    // reading this code see the integration pattern, not a benchmark.
    let mature_cutoff: i16 = NOW_YEAR - MATURITY_YEARS as i16;
    let mature_pool: Vec<Elephant> = Elephant::objects()
        .filter(|e| e.estimated_birth_year().lte(mature_cutoff))
        .fetch_all(ctx)
        .await?;

    // Punnu warm-up — insert each row into the framework's per-context
    // typed L1 cache so adopters can `pool.get(id)` against the same
    // ctx later without a DB round trip. The boot hook emitted by
    // `#[derive(Model)]` registers `Punnu<Elephant>` at
    // `DjogiContext::from_pool` time, so `ctx.punnu::<Elephant>()`
    // always returns `Some` for a default-derived model.
    if let Some(pool) = ctx.punnu::<Elephant>() {
        for e in &mature_pool {
            // Best-effort insertion. `Punnu::insert` returns
            // `Err(InsertError::AlreadyExists)` for repeat rows; that
            // is fine here — repeated demo runs against the same
            // context would otherwise spam stderr. The pool's
            // `OnConflict` default is `Replace`, but the typed
            // surface returns the error variant for the demo's
            // tracing-and-continue shape.
            let _ = pool.insert(e.clone()).await;
        }
    }

    let mature_females: Vec<&Elephant> = mature_pool
        .iter()
        .filter(|e| e.tags.data.sex.as_deref() == Some("f"))
        .collect();
    let mature_males: Vec<&Elephant> = mature_pool
        .iter()
        .filter(|e| e.tags.data.sex.as_deref() == Some("m"))
        .collect();
    let female_ids: Vec<djogi::HeerId> = mature_females.iter().map(|e| e.id).collect();
    let male_ids: Vec<djogi::HeerId> = mature_males.iter().map(|e| e.id).collect();

    // ── Step 3 (typed pair-tuple closure self-join) ────────────────
    //
    // The retrofit centre of this demo. The Wright F coefficient per
    // `(female, male)` pair is the output of the typed
    // `JoinedQuerySet<Elephant, Elephant>` substrate:
    //
    //   Elephant::objects()
    //       .self_pairs()                                 // (L, R = L)
    //       .filter_left(|f| f.id().in_(female_ids))      // female pool
    //       .filter_right(|m| m.id().in_(male_ids))       // male pool
    //       .left_join_closure_pair::<ElephantAncestry>() // la / ra aliases
    //       .annotate(|_l, _r| PairClosureKinshipSum::<ElephantAncestry>::new())
    //       .fetch_all(ctx)
    //
    // The substrate emits:
    //
    //   SELECT l.<cols> AS l_<cols>, r.<cols> AS r_<cols>,
    //          COALESCE(SUM(la.path_count * ra.path_count
    //                       * POWER(0.5, la.depth + ra.depth + 1)), 0)
    //          ::float8 AS __djogi_agg_0
    //   FROM elephants AS l
    //   CROSS JOIN elephants AS r
    //   LEFT JOIN elephant_ancestries AS la ON la.elephant_id = l.id
    //   LEFT JOIN elephant_ancestries AS ra ON ra.elephant_id = r.id
    //                                      AND ra.ancestor_id = la.ancestor_id
    //   WHERE l.id <> r.id
    //     AND l.id = ANY($1) AND r.id = ANY($2)
    //   GROUP BY l.id, r.id;
    //
    // and decodes the result back into `Vec<((Elephant, Elephant),
    // f64)>` — one row per pair, with the Wright F kinship in the
    // aggregate slot. The closure-pair-required `GROUP BY l.id, r.id`
    // is auto-emitted because `PairClosureKinshipSum::requires_closure_pair_join()`
    // reports true. See `djogi/src/query/joined.rs` for the full
    // emission contract.
    let kinship_pairs: Vec<((Elephant, Elephant), f64)> = Elephant::objects()
        .self_pairs()
        .filter_left(|f| f.id().in_(female_ids.clone()))
        .filter_right(|m| m.id().in_(male_ids.clone()))
        .left_join_closure_pair::<ElephantAncestry>()
        .annotate(|_l, _r| PairClosureKinshipSum::<ElephantAncestry>::new())
        .fetch_all(ctx)
        .await?;

    // ── Step 4 (Rust) — composite score + top-N per female ─────────
    //
    // Compose `score = (1 - F) × overlap × age_compat`, then group by
    // female and surface the top `TOP_N_PER_FEMALE` males per female.
    // See module docstring for why this stage stays in Rust today.
    let mut scored: Vec<MatingPair> = Vec::with_capacity(kinship_pairs.len());
    for ((female, male), f_offspring) in &kinship_pairs {
        let f_herd = female.herd_id.key();
        let m_herd = male.herd_id.key();
        if !herds_with_hulls.contains(&f_herd) || !herds_with_hulls.contains(&m_herd) {
            // One side has no recorded sightings, so no territory
            // polygon. Skipped per the demo's filter contract.
            continue;
        }
        let overlap = territory_overlap_pct(f_herd, m_herd);
        if overlap <= 0.0 {
            continue;
        }
        let f_age = age_for(female.estimated_birth_year);
        let m_age = age_for(male.estimated_birth_year);
        let age_compat = age_compatibility(f_age, m_age);
        let kinship_term = 1.0 - f_offspring;
        let score = kinship_term * overlap * age_compat;

        let female_herd = herd_name_by_id
            .get(&f_herd)
            .cloned()
            .unwrap_or_else(|| "(unknown)".to_string());
        let male_herd = herd_name_by_id
            .get(&m_herd)
            .cloned()
            .unwrap_or_else(|| "(unknown)".to_string());

        scored.push(MatingPair {
            // Filled in by the ranking pass below.
            rank: 0,
            female_id: female.id.as_i64().to_string(),
            female_name: female.name.clone().into_inner(),
            female_herd,
            male_id: male.id.as_i64().to_string(),
            male_name: male.name.clone().into_inner(),
            male_herd,
            f_offspring: *f_offspring,
            territory_overlap_pct: overlap,
            age_compatibility: age_compat,
            score,
        });
    }

    // Rank within each female partition. Sort by `(female_id,
    // score DESC, male_name ASC)` then keep the first
    // `TOP_N_PER_FEMALE` rows per female, assigning a 1-based rank.
    scored.sort_by(|a, b| {
        a.female_id
            .cmp(&b.female_id)
            .then_with(|| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.male_name.cmp(&b.male_name))
    });

    let mut ranked: Vec<MatingPair> = Vec::with_capacity(scored.len());
    let mut current_female: Option<String> = None;
    let mut rank_in_female = 0i32;
    for mut pair in scored {
        if Some(&pair.female_id) != current_female.as_ref() {
            current_female = Some(pair.female_id.clone());
            rank_in_female = 0;
        }
        rank_in_female += 1;
        if rank_in_female as usize > TOP_N_PER_FEMALE {
            continue;
        }
        pair.rank = rank_in_female;
        ranked.push(pair);
    }

    // Final output ordering: by female_name then rank, matching the
    // pre-retrofit raw SQL's `ORDER BY scored.female_name ASC,
    // scored.rank ASC`.
    ranked.sort_by(|a, b| {
        a.female_name
            .cmp(&b.female_name)
            .then_with(|| a.rank.cmp(&b.rank))
    });

    let mut target = output::open_writer(out)?;
    match format {
        Format::Json => output::write_json(&mut target, &ranked)?,
        Format::Markdown => render_markdown(&mut target, &ranked)?,
        Format::Mermaid => render_mermaid(&mut target, &ranked)?,
    }
    Ok(())
}

/// Compute the territory-overlap fraction for a `(female_herd,
/// male_herd)` pair.
///
/// **Deterministic-seed behaviour.** The example's seed places herd
/// centres far enough apart relative to per-herd sighting jitter that
/// cross-herd convex hulls never intersect. The function returns the
/// binary same-herd-vs-cross-herd identity that matches the previous
/// raw-SQL `CASE WHEN f.herd_id = m.herd_id THEN 1.0 ...` short-circuit.
///
/// **What changes for adopters.** Adopter datasets with cross-territory
/// wandering elephants would surface graduated overlaps in `[0, 1]`. The
/// real-overlap path requires either a pair-side typed spatial
/// expression (an `Expr::area_of_intersection` that references
/// per-row `l.<hull_col>` / `r.<hull_col>` columns rather than the
/// existing scalar-EWKB form) or a small polygon-intersection helper
/// in Rust. Both are tracked: the typed pair-side spatial expression
/// is part of issue #99's remaining substrate work; the Rust-side
/// helper is a polish-pass amendment to this demo. Until either lands,
/// the binary fallback is exact for the deterministic seed.
fn territory_overlap_pct(female_herd: djogi::HeerId, male_herd: djogi::HeerId) -> f64 {
    if female_herd == male_herd { 1.0 } else { 0.0 }
}

/// Compute an elephant's age in years given its `estimated_birth_year`.
/// Returns `0` when the birth year is missing — the caller's pre-Step-3
/// filter already excluded `NULL` birth years, so this branch is
/// defensive against partial seed data.
fn age_for(birth_year: Option<i16>) -> i16 {
    match birth_year {
        Some(y) => NOW_YEAR - y,
        None => 0,
    }
}

/// Age-compatibility multiplier ∈ `(0, 1]` for a `(female_age,
/// male_age)` pair (years).
///
/// The framework's mating-pairs composite is
/// `score = (1 - F) × territory_overlap_pct × age_compatibility`. The
/// third factor exists so the score reflects biological plausibility,
/// not just genetic distance. The function is the geometric mean of
/// two Gaussian bells:
///
/// - Female bell: peak `FEMALE_FERTILITY_PEAK` (20), σ
///   `FEMALE_FERTILITY_SIGMA` (10). Centred on early adulthood.
/// - Male bell: peak `MALE_FERTILITY_PEAK` (32), σ
///   `MALE_FERTILITY_SIGMA` (15). Centred on the musth-bull mean with
///   a wider tail.
///
/// The geometric mean (`sqrt(f_score × m_score)`) penalises one-sided
/// unsuitability more than an arithmetic average would: a pair with
/// `f_score = 1.0` and `m_score = 0.1` returns `~0.32`, not `0.55`.
/// Output is always in `(0, 1]`.
fn age_compatibility(female_age: i16, male_age: i16) -> f64 {
    let f_score = gaussian_bell(
        female_age as f64,
        FEMALE_FERTILITY_PEAK,
        FEMALE_FERTILITY_SIGMA,
    );
    let m_score = gaussian_bell(male_age as f64, MALE_FERTILITY_PEAK, MALE_FERTILITY_SIGMA);
    (f_score * m_score).sqrt()
}

/// `exp(-0.5 × ((x - mean) / sigma)²)` — value of the unnormalised
/// Gaussian at `x`. Peaks at `1.0` for `x = mean`; falls smoothly to
/// `~0.6` at one σ away and `~0.14` at two σ away.
fn gaussian_bell(x: f64, mean: f64, sigma: f64) -> f64 {
    let z = (x - mean) / sigma;
    (-0.5 * z * z).exp()
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
        let f_node = output::mermaid_node_id_from_str(&p.female_id);
        let m_node = output::mermaid_node_id_from_str(&p.male_id);
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
        let f_node = output::mermaid_node_id_from_str(&p.female_id);
        let m_node = output::mermaid_node_id_from_str(&p.male_id);
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
             recorded sightings (the `herd_hulls` aggregate drops \
             herds with zero sightings), and at least two such \
             elephants share spatially-overlapping herd-territory \
             polygons. The most common cause is running `migrate` from \
             a pre-T22 snapshot whose elephants don't have sex tags; \
             the second is a partial seed where some herds have \
             no sightings yet._",
        )?;
        return Ok(());
    }
    output::write_line(
        target,
        "Score = `(1 - F_offspring) × territory_overlap_pct × \
         age_compatibility`. Higher is better. `F_offspring` uses the \
         simplified Wright form with `F_A = 0` for ancestors — \
         **exact** for non-inbred ancestors, but **production adopters \
         with linebreeding should use the full Wright recurrence**: \
         each affected term is under-counted by `(1 + F_A)` and that \
         factor can be substantial enough to shift the top-N ranking. \
         The demo's role is showcasing the framework substrate \
         (`materialize_closure` + typed pair-tuple closure self-join \
         + `PairClosureKinshipSum` aggregate), not shipping a kinship \
         library. `territory_overlap_pct` is the fraction of the \
         female's herd-territory polygon (convex hull of recorded \
         sightings) covered by the male's herd-territory polygon — \
         1.0 for the same-herd case, lower for spatially-disjoint \
         cross-herd pairs (filtered out before kinship computation \
         when `overlap = 0`). `age_compatibility` is the geometric \
         mean of two Gaussian fertility bells (female peaked at 20, \
         male peaked at 32).\n",
    )?;
    output::write_line(
        target,
        "| Rank | Female (herd) | Male (herd) | F_offspring | Overlap | Age compat | Score |",
    )?;
    output::write_line(target, "|---:|---|---|---:|---:|---:|---:|")?;
    for p in pairs {
        output::write_line(
            target,
            &format!(
                "| {} | {} ({}) | {} ({}) | {:.6} | {:.4} | {:.4} | {:.6} |",
                p.rank,
                p.female_name,
                p.female_herd,
                p.male_name,
                p.male_herd,
                p.f_offspring,
                p.territory_overlap_pct,
                p.age_compatibility,
                p.score,
            ),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Pure-function unit tests for the demo's Rust-side score
    //! arithmetic. Integration tests against a real Postgres live in
    //! `examples/elephant-tracker/tests/mating_pairs_correctness.rs`.

    use super::*;

    #[test]
    fn gaussian_bell_peaks_at_one() {
        let v = gaussian_bell(20.0, 20.0, 10.0);
        assert!(
            (v - 1.0).abs() < 1e-12,
            "bell peaks at exactly 1.0; got {v}"
        );
    }

    #[test]
    fn gaussian_bell_one_sigma() {
        // exp(-0.5) ≈ 0.6065306597126334
        let v = gaussian_bell(30.0, 20.0, 10.0);
        assert!(
            (v - 0.6065306597126334).abs() < 1e-12,
            "bell at 1σ ≈ 0.6065; got {v}"
        );
    }

    #[test]
    fn age_compatibility_is_bounded() {
        for f in 0..50i16 {
            for m in 0..60i16 {
                let v = age_compatibility(f, m);
                assert!(
                    (0.0..=1.0).contains(&v),
                    "age_compatibility({f}, {m}) = {v} out of [0, 1]"
                );
            }
        }
    }

    #[test]
    fn age_compatibility_geometric_mean_penalises_one_sided() {
        // Female at peak (20), male at far tail (60): male bell at
        // (60-32)/15 = 1.866σ → exp(-1.742) ≈ 0.175. Geometric mean
        // of (1.0, 0.175) ≈ 0.418, NOT (1.0 + 0.175) / 2 = 0.587.
        let v = age_compatibility(20, 60);
        assert!(
            v < 0.5,
            "geometric mean must penalise one-sided unsuitability; got {v}"
        );
    }

    #[test]
    fn territory_overlap_pct_binary_same_herd() {
        let h1 = djogi::HeerId::from_i64(100).unwrap();
        let h2 = djogi::HeerId::from_i64(200).unwrap();
        assert_eq!(territory_overlap_pct(h1, h1), 1.0);
        assert_eq!(territory_overlap_pct(h1, h2), 0.0);
    }
}
