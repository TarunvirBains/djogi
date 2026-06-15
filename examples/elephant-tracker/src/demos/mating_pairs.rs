//! `mating-pairs` — Wright F coefficient over the materialized
//! ancestry closure, top-3 candidate pairs per mature female.
//!
//! ## What this demonstrates
//!
//! - **Typed pair-tuple closure self-join** (, GH #99). The
//! ranking previously emitted by a hand-written `WITH RECURSIVE`-flavoured
//! raw SQL block is now expressed through
//! `Elephant::objects().self_pairs().left_join_closure_pair::<ElephantAncestry>().annotate(...)`.
//! No raw SQL is involved in the kinship pass — the
//! `JoinedQuerySet<L, L>` substrate emits the `CROSS JOIN... LEFT JOIN
//! <closure> AS la / ra...` plus the per-pair `GROUP BY l.id, r.id`
//! directly. The Wright kinship sum is a typed annotation slot
//! (`PairClosureKinshipSum<ElephantAncestry>`) that emits
//! `COALESCE(SUM(la.path_count * ra.path_count * 0.5^(la.depth + ra.depth + 1)), 0)::float8`
//! under the framework-reserved `__djogi_agg_0` alias.
//!
//! - **Materialized closure as production-shape kinship lookup.**
//! The `ElephantAncestry` table (populated at seed time by
//! `Model::materialize_closure`) is joined to itself on
//! `ancestor_id` to find every shared ancestor between a female's
//! line and a male's line. One closure read per query rather than
//! re-walking the recursive CTE per pair — this is the framework's
//! answer for adopters at scale.
//!
//! - **Wright 1922 inbreeding coefficient.** For each candidate pair
//! `(female, male)`, the offspring's expected inbreeding
//! coefficient `F` is:
//!
//! ```text
//! F = SUM over common ancestors A of:
//!  (left.path_count × right.path_count) × 0.5^(d_left + d_right + 1)
//! ```
//!
//! where `left` is `(elephant_id = female_id, ancestor_id = A)` and
//! `right` is `(elephant_id = male_id, ancestor_id = A)` from the
//! closure. `path_count` preserves multi-path multiplicity per
//! Wright kinship; the framework's `materialize_closure` is correct
//! on this point because the underlying recursive CTE uses
//! `UNION ALL` (not `UNION`) so paths through different edges to
//! the same ancestor are summed, not deduped.
//!
//! This implementation uses the simplified Wright form with
//! `(1 + F_A) ≈ 1` — i.e., the ancestor's own inbreeding
//! coefficient is treated as zero. The full recursive Wright
//! form would require a self-referential `WITH RECURSIVE` over
//! `F` itself, which is beyond the demo's pedagogical scope.
//!
//! **Caution for adopter use:** for our deterministic 120-elephant
//! seed (matriarchs and bulls are unrelated by construction so
//! ancestors carry no inbreeding) `F_A = 0` is exact. For real-
//! world adopter datasets with inherited inbreeding, each affected
//! term is under-counted by `(1 + F_A)`, and `(1 + F_A)` is not
//! necessarily small — populations with deep linebreeding can
//! carry F_A values that shift the top-N ranking. Any production
//! adopter computing kinship in earnest should extend this query
//! to the full Wright recurrence, or substitute a heavier-weight
//! library such as a published Wright/Malécot implementation.
//! The demo's value here is showing the framework substrate (one
//! `materialize_closure` call + indexed self-join), not shipping
//! a kinship library.
//!
//! - **Top-N per partition.** The Wright F values come back as
//! `Vec<((Elephant, Elephant), f64)>` from the typed pair-tuple
//! terminal. The composite-score multiplication, top-3 ranking, and
//! `(female_name, rank)` sort all run in Rust because the typed
//! pair-tuple `qualify(...)` surface in the substrate
//! accepts only column references on its `partition_by_pair` /
//! `order_by_pair_desc` window-fn methods — not an arbitrary
//! `Expr<f64>` derived from `(1 - F) × overlap × age_compat`. The
//! demo's score is composed from three different sources (typed
//! aggregate output for `F` on `(Elephant, Elephant)` pairs, typed
//! aggregate output for `overlap` on the separate
//! `(Herd, Herd)` pair tuple from `Herd::objects().self_pairs()`
//! with `PairAreaOverlapRatio`, and Rust-side bell-curve for
//! `age_compat`), so the natural place to combine them is Rust. A
//! future slice that adds a pair-side `Expr`-based
//! `order_by_pair_desc` surface would let the entire ranking pass
//! land in SQL; see #99 and #65's "What changes when this ships"
//! note. (The pair-tuple `Expr::area_of_intersection` shape is now
//! shipped as `PairAreaOverlapRatio<L, R>` — see Step 2.5 below.)
//!
//! - **Punnu cache showcase.** Step 2 binds the mature-elephant typed
//! fetch to a `Punnu<Elephant>` L1 pool via the canonical
//! `QuerySet::cache(&pool)?.fetch_all(ctx)` modifier — the typed
//! surface mirrors rows into the identity map at the row-decode
//! boundary, no manual `pool.insert` loop. The demo then performs
//! one observable `pool.get(id)` lookup against the warmed pool so
//! the showcase reads as well as writes: the returned
//! `Arc<Elephant>` is asserted to match the original row and the
//! pool size is surfaced via `tracing::info!` for adopters wiring
//! `tracing-subscriber`. The CLI exits at end-of-process so the
//! pool drops with it; in a long-lived adopter process (request
//! handler, periodic scoring job) the same `pool.get(id)` pattern
//! serves every subsequent lookup without a DB round-trip.
//!
//! - **Pair-tuple spatial overlap retrofit ( #99 closure).**
//! The territory-overlap factor is the typed pair-tuple
//! `PairAreaOverlapRatio<Herd, Herd>` annotation on a
//! `Herd::objects().self_pairs()` join, reading the materialised
//! `Herd.territory` convex-hull polygon (populated at seed time from
//! sighting clusters) on both sides of the pair. The annotation
//! emits
//!
//! ```sql
//! COALESCE(ST_Area(ST_Intersection(l.territory::geometry, r.territory::geometry)::geography), 0)::float8
//!  / NULLIF(ST_Area(l.territory::geography), 0)::float8
//! ```
//!
//! in one SQL pass over every herd-pair, returning
//! `Vec<((Herd, Herd), f64)>`. The pre-retrofit binary
//! same-herd=1.0 / cross-herd=0.0 fallback is gone: same-herd pairs
//! now report `1.0` because both sides reference the same hull
//! (intersection = the hull itself = denominator). Cross-herd pairs
//! report whatever fraction of the female-herd hull spatially
//! overlaps the male-herd hull — non-binary `(0, 1)` values surface
//! when adopter herds have wandering elephants whose sightings
//! straddle two herd ranges.
//!
//! ## Composite score
//!
//! The v3 plan calls for a multi-factor score:
//!
//! ```text
//! score = (1 - F) × territory_overlap_pct × age_compatibility
//! ```
//!
//! All three factors ship here.
//!
//! - **Kinship `(1 - F)`** comes from the typed pair-tuple
//! closure self-join above (Step 3).
//! - **`territory_overlap_pct`** comes from the typed
//! `PairAreaOverlapRatio<Herd, Herd>` annotation on a
//! `Herd::objects().self_pairs()` join (Step 2.5). The
//! pre-retrofit binary same-herd / cross-herd identity is replaced
//! by a real `ST_Area(ST_Intersection(...)) / ST_Area(left)` ratio
//! computed in one SQL pass. The decoded value is in `[0, 1]`:
//! `1.0` for same-herd or fully-coincident territories, `0.0` for
//! fully-disjoint, and any fraction in between for partial
//! overlap. The composite-score arithmetic gates kinship × age on
//! this ratio so a perfectly-compatible cross-herd pair with no
//! territorial overlap still scores zero (they cannot physically
//! meet).
//! - **`age_compatibility`** is a smooth fertility-window product
//! over female age peaked at 20 (sigma 10) and male age peaked at
//! 32 (sigma 15); we report the geometric mean of the two bells so
//! a one-sided unsuitability is penalised more than a single-
//! factor average would be.
//!
//! ## Output formats
//!
//! - `json` — flat list of `{rank, female_id, female_name,
//! female_herd, male_id, male_name, male_herd, f_offspring,
//! territory_overlap_pct, age_compatibility, score}` records,
//! top-3 per female, sorted by `(female_name, rank)`.
//! - `markdown` — sorted table with one row per pair plus a
//! summary header.
//! - `mermaid` — `graph LR` with one node per participating elephant
//! (label includes herd) and one directed `female --> male` edge
//! per pair (label `#rank score=N.NNN`).
//!
//! Filter: only mature elephants (estimated_birth_year ≤
//! `now - MATURITY_YEARS years`, see the constant for rationale on
//! the threshold) of opposite sex. Pairs whose herd territories do
//! not overlap at all (`territory_overlap_pct = 0.0`) drop out
//! after kinship computation — there is no plausible meeting under
//! the herd-territory model. Pairs whose territories partially
//! overlap (the cross-herd wandering case) survive at a
//! correspondingly-discounted score.

use anyhow::Result;
use djogi::DjogiContext;
use djogi::prelude::*;
use djogi::query::{PairAreaOverlapRatio, PairClosureKinshipSum};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

use crate::models::{Elephant, ElephantAncestry, Herd};
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
    /// Multiplier in `[0, 1]` that gates the kinship × age product
    /// on territorial co-occurrence — the fraction of the female
    /// herd's territory that overlaps the male herd's territory.
    ///
    /// Computed by the typed pair-tuple
    /// `PairAreaOverlapRatio<Herd, Herd>` annotation on a
    /// `Herd::objects().self_pairs().include_equal_pk()` join: the
    /// SQL emits `ST_Area(ST_Intersection(l.territory, r.territory)) /
    /// ST_Area(l.territory)` over the materialised `Herd.territory`
    /// convex-hull polygon (populated at seed time from sighting
    /// clusters). `1.0` for same-herd or perfectly-coincident
    /// territories; `0.0` for fully-disjoint; any fraction in
    /// between for partial overlap.
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
/// 1. Fetch the per-herd label lookup (one typed `Herd::objects()`).
/// 2. Fetch mature elephants via the typed
/// `Elephant::objects().filter(...).cache(&pool)?.fetch_all(ctx)`
/// cache-bound terminal — rows land in the `Punnu<Elephant>` L1
/// pool at row-decode time. Demonstrate an observable
/// `pool.get(id)` hit immediately after.
/// 3. Fetch every `(female, male)` mature pair with its Wright F
/// coefficient via the typed
/// `Elephant::objects().self_pairs().left_join_closure_pair::<ElephantAncestry>().annotate(PairClosureKinshipSum)`
/// chain. No raw SQL.
/// 4. Compute composite score in Rust (overlap × kinship × age
/// compatibility), rank top-N per female, render.
pub async fn run(ctx: &mut DjogiContext, format: Format, out: Option<&Path>) -> Result<()> {
    // ── Step 1 (typed Djogi) — per-herd label lookup ──────────────
    //
    // One typed fetch over `Herd::objects()` powers the output
    // rows' `female_herd` / `male_herd` columns. No JOIN through
    // raw SQL, no per-pair lookup round trip.
    //
    // The convex-hull computation that used to live in this step's
    // earlier draft has moved to seed time
    // (`seed::populate_herd_territories`) and persists in the
    // `Herd.territory` column. Step 2.5 below reaches the persisted
    // polygons through `Herd::objects()` and feeds them to the
    // pair-tuple territory-overlap surface in one SQL pass.
    let herds: Vec<Herd> = Herd::objects().fetch_all(ctx).await?;
    let herd_name_by_id: HashMap<djogi::HeerId, String> =
        herds.iter().map(|h| (h.id, h.name.clone())).collect();

    // ── Step 2 (typed Djogi + Punnu cache showcase) ───────────────
    //
    // Canonical `.cache(&pool)?.fetch_all(ctx)` shape — the cache
    // binding is opt-in on the queryset, the row-decode pipeline
    // upserts each row into the bound `Punnu<Elephant>` pool, and the
    // queryset is required to be portable (the `lte` filter here is).
    // No manual `pool.insert` loop; row→pool mirroring happens at the
    // row-decode boundary inside djogi's existing terminal pipeline.
    //
    // `#[derive(Model)]` auto-emits `impl Cacheable for Elephant`, and
    // the boot hook registers `Punnu<Elephant>` at `DjogiContext::
    // from_pool` time — so `ctx.punnu::<Elephant>()` returns `Some` on
    // any default-derived model. The hook machinery (cluster 8δ T7.5)
    // also invalidates rows on `Elephant::create` / `save` / `delete`
    // so adopters do not maintain a manual write-through.
    //
    // The block below adds the cache-bound fetch and one observable
    // `pool.get(id)` lookup so the showcase reads as well as writes.
    // Without an observable read the showcase would be invisible at
    // runtime — strict-swe (#108 review) blocked the previous shape on
    // exactly this point.
    let mature_cutoff: i16 = NOW_YEAR - MATURITY_YEARS as i16;
    let mature_pool: Vec<Elephant> = match ctx.punnu::<Elephant>() {
        Some(pool) => {
            let pool = pool.clone();
            let rows = Elephant::objects()
                .filter(|e| e.estimated_birth_year().lte(mature_cutoff))
                .cache(&pool)
                .map_err(|(_qs, err)| {
                    anyhow::anyhow!(
                        "QuerySet::cache(&punnu) rejected the mature-elephant filter \
       as non-portable: {err:?}",
                    )
                })?
                .fetch_all(ctx)
                .await?;

            // Observable cache read — `pool.get(id)` is a synchronous
            // L1 lookup that returns `Arc<Elephant>` for any id the
            // cache-bound fetch above mirrored in. We sample the
            // first row, verify the `Arc<Elephant>` round-trips with
            // the same id (cache miss would return `None`, write-
            // through bug would return a stale clone with a
            // different id), and surface the pool size via
            // `tracing::info!` so adopters wiring
            // `tracing-subscriber` see the showcase in their logs.
            if let Some(sample) = rows.first() {
                let cached: std::sync::Arc<Elephant> = pool.get(&sample.id).expect(
                    "Punnu<Elephant> cache miss for an id we just \
       cache-bound through.cache(&pool)?.fetch_all — \
       either the cache-binding pipeline did not \
       mirror rows or the pool was invalidated \
       between fetch_all and the get(...) call",
                );
                debug_assert_eq!(
                    cached.id, sample.id,
                    "Punnu<Elephant> returned a different row id than \
      the sample we keyed on — cache identity violation",
                );
                tracing::info!(
                 cache_size = pool.len(),
                 sample_id = ?sample.id,
                 sample_name = %cached.name.clone().into_inner(),
                 "Punnu<Elephant> warm — `.cache(&pool)?` mirrored the \
                  mature-elephant fetch, observable `pool.get(id)` \
                  returned Arc<Elephant>"
                );
            } else {
                tracing::info!(
                    cache_size = pool.len(),
                    "Punnu<Elephant> warm — no mature elephants matched \
      the filter; cache stays empty"
                );
            }

            rows
        }
        None => {
            // Punnu integration is opt-in at the `DjogiContext`
            // builder level. Adopters who disable it (or stub it out
            // in a custom builder) still get the typed fetch
            // unchanged — the cache showcase is documented in the
            // module docstring as the value-add path.
            tracing::info!(
                "Punnu<Elephant> not registered on this DjogiContext; \
     skipping cache showcase and falling back to the bare \
     typed fetch"
            );
            Elephant::objects()
                .filter(|e| e.estimated_birth_year().lte(mature_cutoff))
                .fetch_all(ctx)
                .await?
        }
    };

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

    // ── Step 2.5 (typed pair-tuple territory overlap) ──────────────
    //
    // For every `(female_herd, male_herd)` pair compute the fraction
    // of the female herd's territory polygon that overlaps the male
    // herd's territory polygon. Emits one SQL pass via the
    // `PairAreaOverlapRatio<Herd, Herd>` typed annotation:
    //
    // Herd::objects()
    // .self_pairs()
    // .include_equal_pk() // same-herd pairs report 1.0 here
    // .annotate(|l, r| PairAreaOverlapRatio::new(l.territory(), r.territory()))
    // .fetch_all(ctx)
    //
    // The substrate emits:
    //
    // SELECT l.*, r.*,
    //   COALESCE(ST_Area(ST_Intersection(l.territory::geometry,
    //           r.territory::geometry)::geography), 0)::float8
    //   / NULLIF(ST_Area(l.territory::geography), 0)::float8
    //   AS __djogi_agg_0
    // FROM herds AS l CROSS JOIN herds AS r;
    //
    // and decodes the result back into `Vec<((Herd, Herd), f64)>` —
    // one row per herd-pair with the territory-overlap ratio. The
    // `include_equal_pk()` modifier opts in to the diagonal so
    // same-herd pairs surface a clean `1.0` (perfectly coincident
    // territories — same polygon on both sides). NULL territory on
    // either side yields a NULL ratio which the slot decodes as 0.0.
    let overlap_pairs: Vec<((Herd, Herd), f64)> = Herd::objects()
        .self_pairs()
        .include_equal_pk()
        .annotate(|l, r| PairAreaOverlapRatio::new(l.territory(), r.territory()))
        .fetch_all(ctx)
        .await?;
    let mut overlap_by_herd_pair: HashMap<(djogi::HeerId, djogi::HeerId), f64> =
        HashMap::with_capacity(overlap_pairs.len());
    for ((l_herd, r_herd), ratio) in overlap_pairs {
        // `ST_Area(ST_Intersection(...))/ST_Area(l)` can drift
        // marginally above 1.0 for fully-coincident geographies due
        // to floating-point on the geography spheroid; clamp into
        // `[0, 1]` so the downstream score keeps the `[0, 1]`
        // contract for adopters wiring this output into UI gauges.
        overlap_by_herd_pair.insert((l_herd.id, r_herd.id), ratio.clamp(0.0, 1.0));
    }

    // ── Step 3 (typed pair-tuple closure self-join) ────────────────
    //
    // The retrofit centre of this demo. The Wright F coefficient per
    // `(female, male)` pair is the output of the typed
    // `JoinedQuerySet<Elephant, Elephant>` substrate:
    //
    // Elephant::objects()
    // .self_pairs()         // (L, R = L)
    // .filter_left(|f| f.id().in_(female_ids))  // female pool
    // .filter_right(|m| m.id().in_(male_ids))  // male pool
    // .left_join_closure_pair::<ElephantAncestry>() // la / ra aliases
    // .annotate(|_l, _r| PairClosureKinshipSum::<ElephantAncestry>::new())
    // .fetch_all(ctx)
    //
    // The substrate emits:
    //
    // SELECT l.<cols> AS l_<cols>, r.<cols> AS r_<cols>,
    //   COALESCE(SUM(la.path_count * ra.path_count
    //      * POWER(0.5, la.depth + ra.depth + 1)), 0)
    //   ::float8 AS __djogi_agg_0
    // FROM elephants AS l
    // CROSS JOIN elephants AS r
    // LEFT JOIN elephant_ancestries AS la ON la.elephant_id = l.id
    // LEFT JOIN elephant_ancestries AS ra ON ra.elephant_id = r.id
    //          AND ra.ancestor_id = la.ancestor_id
    // WHERE l.id <> r.id
    //  AND l.id = ANY($1) AND r.id = ANY($2)
    // GROUP BY l.id, r.id;
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
        let overlap = overlap_by_herd_pair
            .get(&(f_herd, m_herd))
            .copied()
            .unwrap_or(0.0);
        if overlap <= 0.0 {
            // Fully-disjoint herds — they cannot physically meet
            // under the herd-territory model. Drop the pair before
            // multiplying through the rest of the score.
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
/// `FEMALE_FERTILITY_SIGMA` (10). Centred on early adulthood.
/// - Male bell: peak `MALE_FERTILITY_PEAK` (32), σ
/// `MALE_FERTILITY_SIGMA` (15). Centred on the musth-bull mean with
/// a wider tail.
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
            " n0[\"No candidate pairs — verify seed has mature \
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
        output::write_line(target, &format!(" {id}[\"{label}\"]"))?;
    }
    for p in pairs {
        let f_node = output::mermaid_node_id_from_str(&p.female_id);
        let m_node = output::mermaid_node_id_from_str(&p.male_id);
        output::write_line(
            target,
            &format!(
                " {f_node} -->|\"#{} score={:.3}\"| {m_node}",
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
    populated, `estimated_birth_year` set) and at least two \
    such elephants share a herd. The most common cause is \
    running `migrate` from a pre-T22 snapshot whose \
    elephants don't have sex tags._",
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
   library. `territory_overlap_pct` is the typed pair-tuple \
   `PairAreaOverlapRatio<Herd, Herd>` annotation on a \
   `Herd::objects().self_pairs()` join — emits \
   `ST_Area(ST_Intersection(l.territory, r.territory))/ST_Area(l.territory)` \
   in one SQL pass, with `Herd.territory` populated at seed \
   time as the convex hull of each herd's sightings. \
   `age_compatibility` is the geometric mean of two Gaussian \
   fertility bells (female peaked at 20, male peaked at 32).\n",
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

    // Territory overlap is now computed by Postgres via the typed
    // `PairAreaOverlapRatio<Herd, Herd>` annotation (see Step 2.5 of
    // `run`). There is no Rust-side `territory_overlap_pct` function
    // to unit-test in this module; the pair-side SQL emission is
    // pinned by `djogi::query::joined::tests::pair_area_overlap_*` and
    // the end-to-end behaviour is exercised by the elephant-tracker
    // `cargo run -- demo mating-pairs --format json` workflow.
}
