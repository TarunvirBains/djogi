//! Kinship calculation benchmark — closes GH #85 T27 part 1.
//!
//! # What this fixture pins
//!
//! The mating-pairs demo's substrate claim is that the materialised
//! pedigree closure (`Elephant::materialize_closure::<ElephantAncestry>`)
//! is the production-shape answer for kinship scoring: one closure
//! pass written ahead of time, then `O(N²)` typed pair-tuple queries
//! that join the closure twice with constant per-pair cost. The
//! alternative — walking a recursive CTE per candidate pair — pays
//! the recursive walk for every pair, which scales worse than linearly.
//!
//! This fixture confirms the claim with a small representative
//! pedigree (200 elephants across 5 generationerations) and 100 candidate
//! pairs. It is intentionally a correctness-and-shape benchmark: we
//! assert that the closure-based path produces identical Wright F
//! values to the recursive-CTE shape, and we record both runtimes
//! via `tracing::info!` so a CI-side comparison or local profiling
//! run has the data without having to invoke `cargo bench`. The full
//! 5000-elephant production-scale bench lives as a future slice on
//! `cargo bench`; this fixture's role is regression-pinning the
//! closure-vs-recursive-CTE equivalence and surfacing the timing for
//! audit.
//!
//! # Why an integration test, not a Criterion bench
//!
//! Two reasons:
//!
//! 1. **Per-test database**: the fixture needs a fresh database for
//!    deterministic timing baselines; Criterion has no built-in
//!    machinery for that. `#[djogi_test]` gives a fresh DB per test
//!    and tears it down after, isolating the bench's setup cost.
//! 2. **Correctness gate**: the bench is most useful as a regression
//!    pin — "the closure path produces the same Wright F as the
//!    recursive CTE". An integration test asserts that; a Criterion
//!    bench records timing only.
//!
//! Adopters who want a production-scale `cargo bench` shape can copy
//! this fixture's setup into a `benches/` Criterion module; the
//! shape is intentionally portable.

use djogi::DjogiContext;
use djogi::prelude::*;
use djogi::query::{MaterializeClosureOptions, PairClosureKinshipSum};
use elephant_tracker::models::{Elephant, ElephantAncestry, ElephantTags, Herd};
use std::time::Instant;

/// Pedigree size — 5 generationerations of 40 elephants each.
///
/// Smaller than the issue's 5000-elephant target so the live-Postgres
/// CI gate stays under 60s. Adopters scaling this fixture for their
/// own production budgets should bump `GENERATIONS` × `PER_GENERATION`
/// up to whatever their CI tolerates. The closure-vs-recursive-CTE
/// scaling ratio is shape-invariant; the absolute timings shift with
/// hardware but the ratio holds.
const GENERATIONS: usize = 5;
const PER_GENERATION: usize = 40;
const TOTAL_ELEPHANTS: usize = GENERATIONS * PER_GENERATION;

/// Number of candidate pairs to score in the bench loop. Picked so
/// the per-pair overhead is dominated by Postgres query latency
/// rather than Rust-side iteration.
const CANDIDATE_PAIRS: usize = 50;

/// Build a generationeric elephant with a deterministic name and the given
/// parent / sex / herd metadata. Fields the bench doesn't care about
/// are defaulted so the pedigree shape is the only meaningful axis.
fn make_elephant(
    name: &str,
    herd_id: djogi::HeerId,
    mother: Option<djogi::HeerId>,
    father: Option<djogi::HeerId>,
    sex: &str,
) -> Elephant {
    let tags = ElephantTags {
        sex: Some(sex.to_string()),
        ..ElephantTags::default()
    };
    Elephant {
        id: <djogi::HeerId as djogi::PrimaryKey>::sentinel(),
        created_at: djogi::DateTime::UNIX_EPOCH,
        updated_at: djogi::DateTime::UNIX_EPOCH,
        name: Tracked::new(name.to_string()),
        herd_id: ForeignKey::new(herd_id),
        mother_id: mother.map(ForeignKey::new),
        father_id: father.map(ForeignKey::new),
        estimated_birth_year: Some(2000),
        tags: Jsonb::new(tags),
        version: 0,
    }
}

/// Deterministic LCG so the pedigree shape is reproducible across
/// runs. No `rand` dependency — same pattern as `seed::Lcg`.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Lcg(seed | 1)
    }

    fn next_index(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 32) as usize) % bound.max(1)
    }
}

/// Build a 200-elephant pedigree spanning 5 generationerations.
///
/// Each generationeration has `PER_GENERATION` elephants split evenly into
/// females and males. Generation N (N >= 1) picks each elephant's
/// parents from random members of generationeration N-1 (mother from
/// females, father from males). Generation 0 has no parents — the
/// founder pool. Returns the vector of every elephant's id in
/// generationeration order so downstream pair-selection can address them
/// deterministically.
async fn build_pedigree(ctx: &mut DjogiContext, herd_id: djogi::HeerId) -> Vec<djogi::HeerId> {
    let mut rng = Lcg::new(0xC0FFEE);
    let mut ids: Vec<djogi::HeerId> = Vec::with_capacity(TOTAL_ELEPHANTS);
    let mut prev_females: Vec<djogi::HeerId> = Vec::new();
    let mut prev_males: Vec<djogi::HeerId> = Vec::new();

    for generation in 0..GENERATIONS {
        let mut generation_females: Vec<djogi::HeerId> = Vec::with_capacity(PER_GENERATION / 2);
        let mut generation_males: Vec<djogi::HeerId> = Vec::with_capacity(PER_GENERATION / 2);

        for i in 0..PER_GENERATION {
            let sex = if i.is_multiple_of(2) { "f" } else { "m" };
            let (mother, father) = if generation == 0 {
                (None, None)
            } else {
                let m = if prev_females.is_empty() {
                    None
                } else {
                    Some(prev_females[rng.next_index(prev_females.len())])
                };
                let f = if prev_males.is_empty() {
                    None
                } else {
                    Some(prev_males[rng.next_index(prev_males.len())])
                };
                (m, f)
            };
            let name = format!("G{generation}-{i:02}");
            let e = Elephant::create(ctx, make_elephant(&name, herd_id, mother, father, sex))
                .await
                .expect("Elephant::create must succeed during pedigree build");
            ids.push(e.id);
            if sex == "f" {
                generation_females.push(e.id);
            } else {
                generation_males.push(e.id);
            }
        }
        prev_females = generation_females;
        prev_males = generation_males;
    }
    ids
}

/// Score `CANDIDATE_PAIRS` candidate pairs via the typed pair-tuple
/// closure self-join — the production-shape path.
///
/// Returns `Vec<f64>` of Wright F values plus the total wall-clock
/// duration of the entire pair-tuple terminal (one round-trip
/// regardless of pair count).
async fn closure_path_scores(
    ctx: &mut DjogiContext,
    candidate_pairs: &[(djogi::HeerId, djogi::HeerId)],
) -> (Vec<f64>, std::time::Duration) {
    // Build the female / male id pools from the candidate pairs.
    let female_ids: Vec<djogi::HeerId> = candidate_pairs.iter().map(|(l, _)| *l).collect();
    let male_ids: Vec<djogi::HeerId> = candidate_pairs.iter().map(|(_, r)| *r).collect();
    let start = Instant::now();
    let rows: Vec<((Elephant, Elephant), f64)> = Elephant::objects()
        .self_pairs()
        .filter_left(move |f| f.id().in_(female_ids.clone()))
        .filter_right(move |m| m.id().in_(male_ids.clone()))
        .left_join_closure_pair::<ElephantAncestry>()
        .annotate(|_l, _r| PairClosureKinshipSum::<ElephantAncestry>::new())
        .fetch_all(ctx)
        .await
        .expect("closure-path pair-tuple kinship query must execute");
    let duration = start.elapsed();
    // Map rows back to the candidate-pair lookup order. The query
    // returns every (female, male) combination from the two id
    // pools, so we index by (l.id, r.id) to pick the requested
    // candidate pairs.
    let mut by_pair: std::collections::HashMap<(djogi::HeerId, djogi::HeerId), f64> =
        std::collections::HashMap::new();
    for ((l, r), f) in rows {
        by_pair.insert((l.id, r.id), f);
    }
    let scores: Vec<f64> = candidate_pairs
        .iter()
        .map(|(l, r)| by_pair.get(&(*l, *r)).copied().unwrap_or(0.0))
        .collect();
    (scores, duration)
}

/// Score `CANDIDATE_PAIRS` candidate pairs via per-pair recursive-CTE
/// scans — the deliberately-naive path the closure replaces.
///
/// One round-trip per pair, each walking the recursive ancestor
/// chain anew. Returns `Vec<f64>` of Wright F values plus the
/// cumulative wall-clock duration.
///
/// Implemented via `ctx.raw_scalar` because the recursive-CTE shape
/// is intentionally outside djogi's typed surface — the closure-based
/// path is the typed surface, and the recursive-CTE comparison is
/// the "what happens if you don't materialise" reference point. The
/// raw SQL is bind-parameterised (no string interpolation of ids).
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): bench fixture — measures the raw-recursive-CTE alternative
// to the typed closure path; not part of the typed adopter surface djogi recommends.
async fn recursive_cte_path_scores(
    ctx: &mut DjogiContext,
    candidate_pairs: &[(djogi::HeerId, djogi::HeerId)],
) -> (Vec<f64>, std::time::Duration) {
    // Per-pair recursive CTE walking `mother_id` and `father_id`
    // self-FKs, summing path-count-weighted shared-ancestor
    // contributions. UNION ALL preserves path multiplicity, matching
    // `materialize_closure`'s discipline.
    //
    // The two parent edges are combined inside the recursive term via
    // `CROSS JOIN LATERAL (VALUES (mother_id), (father_id))` so the CTE
    // has exactly one base case + one recursive term — Postgres's
    // recursive WITH grammar only admits a single `non_recursive_term
    // UNION ALL recursive_term` split; three branches glued by two
    // `UNION ALL`s are parsed left-associatively, which would place a
    // self-reference inside the non-recursive term and fail with
    // `42P19 recursive reference to query "..." must not appear within
    // its non-recursive term`.
    //
    // Depth-0 anchor rows are KEPT in the final SUM (no
    // `WHERE depth > 0` filter): the textbook Wright F kinship formula
    // counts an individual as its own depth-0 ancestor, so when the
    // candidate pair is ancestor-descendant (e.g. parent × child) the
    // depth-0 row on the ancestor side joins the strict-ancestor row
    // on the descendant side and contributes `(1/2)^(0 + d + 1)` —
    // exactly 0.25 for a parent × child pair. The closure path in
    // [`closure_path_scores`] does not filter depth > 0 either (the
    // `PairClosureKinshipSum<C>` aggregate sums all closure rows),
    // and the two paths must agree to within `1e-9` for the bench's
    // equivalence assertion. Filtering depth > 0 here was the source
    // of an off-by-0.25 disagreement on ancestor-descendant pairs the
    // 200-elephant pedigree surfaces by chance.
    const SQL: &str = "
        WITH RECURSIVE
        left_anc(elephant_id, ancestor_id, depth, path_count) AS (
            SELECT id, id, 0, 1
              FROM elephants
             WHERE id = $1
            UNION ALL
            SELECT la.elephant_id, parent.parent_id, la.depth + 1, la.path_count
              FROM left_anc la
              JOIN elephants e ON e.id = la.ancestor_id
              CROSS JOIN LATERAL (VALUES (e.mother_id), (e.father_id))
                                AS parent(parent_id)
             WHERE parent.parent_id IS NOT NULL AND la.depth < 5
        ),
        right_anc(elephant_id, ancestor_id, depth, path_count) AS (
            SELECT id, id, 0, 1
              FROM elephants
             WHERE id = $2
            UNION ALL
            SELECT ra.elephant_id, parent.parent_id, ra.depth + 1, ra.path_count
              FROM right_anc ra
              JOIN elephants e ON e.id = ra.ancestor_id
              CROSS JOIN LATERAL (VALUES (e.mother_id), (e.father_id))
                                AS parent(parent_id)
             WHERE parent.parent_id IS NOT NULL AND ra.depth < 5
        )
        SELECT COALESCE(SUM(
            la.path_count::numeric * ra.path_count::numeric
            * POWER(0.5::numeric, (la.depth + ra.depth + 1)::numeric)
        ), 0)::float8
          FROM left_anc la
          JOIN right_anc ra ON ra.ancestor_id = la.ancestor_id";

    let start = Instant::now();
    let mut scores = Vec::with_capacity(candidate_pairs.len());
    for (l, r) in candidate_pairs {
        let lpk = l.as_i64();
        let rpk = r.as_i64();
        let f: f64 = ctx
            .raw_scalar(SQL, &[&lpk, &rpk])
            .await
            .expect("recursive-CTE kinship must execute");
        scores.push(f);
    }
    let duration = start.elapsed();
    (scores, duration)
}

#[djogi::djogi_test(
    extensions = ["postgis"],
    sync_models = [Herd, Elephant, ElephantAncestry],
)]
async fn closure_and_recursive_cte_agree_on_kinship_for_200_elephants(
    mut ctx: djogi::DjogiContext,
) {
    // ── Setup ─────────────────────────────────────────────────────
    let herd = Herd::create(
        &mut ctx,
        Herd {
            name: "BenchHerd".to_string(),
            estimated_population: 0,
            territory: None,
            ..Default::default()
        },
    )
    .await
    .expect("herd seed must succeed");
    let ids = build_pedigree(&mut ctx, herd.id).await;
    assert_eq!(
        ids.len(),
        TOTAL_ELEPHANTS,
        "pedigree size must equal {TOTAL_ELEPHANTS}"
    );

    // Materialize the closure once for the entire test. This is the
    // cost the closure-based path amortises across all subsequent
    // pair-scoring queries — the recursive-CTE alternative pays a
    // per-pair recursive walk and never benefits from this
    // up-front investment.
    let materialize_start = Instant::now();
    let report = Elephant::materialize_closure::<ElephantAncestry>(
        &mut ctx,
        MaterializeClosureOptions::default().with_max_depth(5),
    )
    .await
    .expect("closure materialization must succeed");
    let materialize_duration = materialize_start.elapsed();
    tracing::info!(
        rows_written = report.rows_written,
        sources_visited = report.sources_visited,
        materialize_ms = materialize_duration.as_millis() as u64,
        "materialized ElephantAncestry closure"
    );

    // Select `CANDIDATE_PAIRS` deterministic (female, male) pairs
    // from later generationerations (so most have non-trivial shared
    // ancestry). Generations 3 + 4 surface the depth-2 / depth-3
    // shared-ancestor case the closure is supposed to compress.
    let mut rng = Lcg::new(0xBEEF);
    let mut candidate_pairs: Vec<(djogi::HeerId, djogi::HeerId)> =
        Vec::with_capacity(CANDIDATE_PAIRS);
    // Generations 3 + 4 ids — the last two of the 5 (0-indexed).
    let late_start = 3 * PER_GENERATION;
    let late_end = TOTAL_ELEPHANTS;
    let late_pool = &ids[late_start..late_end];
    for _ in 0..CANDIDATE_PAIRS {
        let l = late_pool[rng.next_index(late_pool.len())];
        let mut r = late_pool[rng.next_index(late_pool.len())];
        // Disallow self-pair — kinship-sum aggregate is meaningful
        // only for distinct elephants.
        while r == l {
            r = late_pool[rng.next_index(late_pool.len())];
        }
        candidate_pairs.push((l, r));
    }

    // ── Score via both paths ─────────────────────────────────────
    let (closure_scores, closure_duration) = closure_path_scores(&mut ctx, &candidate_pairs).await;
    let (recursive_scores, recursive_duration) =
        recursive_cte_path_scores(&mut ctx, &candidate_pairs).await;

    tracing::info!(
        candidate_pairs = candidate_pairs.len(),
        closure_ms = closure_duration.as_millis() as u64,
        recursive_cte_ms = recursive_duration.as_millis() as u64,
        materialize_ms = materialize_duration.as_millis() as u64,
        "kinship-bench timings (closure path vs raw recursive-CTE)"
    );

    // ── Correctness gate — both paths must produce identical F ─
    //
    // The closure-based path and the recursive-CTE path implement
    // the same Wright kinship formula over the same edge set; any
    // disagreement means one path has drifted from the textbook
    // definition. Tolerance is `1e-12` to accommodate float8
    // round-trip differences.
    assert_eq!(
        closure_scores.len(),
        recursive_scores.len(),
        "score-vec lengths must match"
    );
    let mut max_diff: f64 = 0.0;
    for (i, (cf, rf)) in closure_scores
        .iter()
        .zip(recursive_scores.iter())
        .enumerate()
    {
        let diff = (cf - rf).abs();
        if diff > max_diff {
            max_diff = diff;
        }
        assert!(
            diff < 1e-9,
            "closure vs recursive-CTE disagreement on pair {i}: \
             closure={cf}, recursive_cte={rf}, diff={diff}"
        );
    }
    tracing::info!(
        max_diff = max_diff,
        "closure-vs-recursive-CTE max disagreement (must be < 1e-9)"
    );
}
