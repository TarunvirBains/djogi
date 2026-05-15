// Phase 8.5 Cluster 4A (#85 T27) — Wright F-coefficient correctness
// fixture for the typed pair-tuple closure self-join surface.
//
// What this file pins:
//
// 1. `Elephant::objects().self_pairs().left_join_closure_pair::<…>()
//    .annotate(PairClosureKinshipSum::<…>::new()).fetch_all(ctx)` produces
//    the SAME Wright F values the demo's now-retrofitted Step 3 emits,
//    against KNOWN reference values from population genetics.
//
// 2. Reference matings covered (offspring F coefficient, from Wright 1922):
//
//        Pair                         Expected F      Where computed
//        ─────────────────────────────────────────────────────────────────
//        Unrelated                    0.000           No common ancestor
//        Parent × child               0.250           parent is common at d=0
//        Full siblings                0.250           two common ancestors d=1
//        Half siblings (one parent)   0.125           one common ancestor d=1
//        First cousins                0.0625          two common ancestors d=2
//
//    These are demo-side correctness anchors flagged in Risk 9 of the
//    Cluster D v3 plan (gh#85). They live alongside the framework's
//    pair-tuple substrate tests (`phase8_zero_materialize_closure_live.rs`)
//    rather than under `examples/elephant-tracker/tests/` because the
//    fixture's payload is the typed pair-tuple closure self-join — a
//    framework substrate concern — and the same Postgres + djogi_test
//    harness the other phase8_zero_* live tests use applies directly.
//    The demo's end-to-end JSON-output behavioural validation is a
//    separate slice tracked in the same issue.
//
// # Why a minimal local pedigree model
//
// The `Elephant` model in the elephant-tracker example carries
// `Tracked<String>`, `Jsonb<ElephantTags>`, optimistic locking, and other
// framework-feature surface that is irrelevant to the F-coefficient
// arithmetic. A minimal `MatingNode` pedigree model in this test file
// makes the seed step ~5 lines per node and lets every assertion fit on
// one screen. The closure self-join + `PairClosureKinshipSum` substrate
// exercised is byte-identical to what the demo emits.
//
// # SQL emitted under the hood (recap)
//
//     SELECT l.<cols> AS l_<cols>, r.<cols> AS r_<cols>,
//            COALESCE(SUM(la.path_count * ra.path_count
//                         * POWER(0.5, la.depth + ra.depth + 1)), 0)
//            ::float8 AS __djogi_agg_0
//     FROM phase8_5_c4a_mating_nodes AS l
//     CROSS JOIN phase8_5_c4a_mating_nodes AS r
//     LEFT JOIN phase8_5_c4a_mating_ancestries AS la ON la.node_id = l.id
//     LEFT JOIN phase8_5_c4a_mating_ancestries AS ra ON ra.node_id = r.id
//                                                    AND ra.ancestor_id = la.ancestor_id
//     WHERE l.id <> r.id
//       AND l.id = ANY($n) AND r.id = ANY($m)
//     GROUP BY l.id, r.id

use djogi::prelude::*;
use djogi::query::PairClosureKinshipSum;

// ── Pedigree model + closure model ──────────────────────────────────────────

#[model(table = "phase8_5_c4a_mating_nodes", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct MatingNode {
    pub label: String,
    pub mother_id: Option<ForeignKey<MatingNode>>,
    pub father_id: Option<ForeignKey<MatingNode>>,
}

#[model(
    table = "phase8_5_c4a_mating_ancestries",
    pk = HeerId,
    no_default,
    indexes(unique(fields = [node_id, ancestor_id, depth]))
)]
#[derive(Debug, Clone)]
pub struct MatingAncestry {
    pub node_id: ForeignKey<MatingNode>,
    pub ancestor_id: ForeignKey<MatingNode>,
    pub depth: i32,
    pub path_count: i64,
}

impl djogi::query::ClosureModel for MatingAncestry {
    type Source = MatingNode;
    fn source_column() -> &'static str {
        "node_id"
    }
    fn ancestor_column() -> &'static str {
        "ancestor_id"
    }
    fn depth_column() -> &'static str {
        "depth"
    }
    fn path_count_column() -> &'static str {
        "path_count"
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

async fn seed(
    ctx: &mut DjogiContext,
    label: &str,
    mother: Option<&MatingNode>,
    father: Option<&MatingNode>,
) -> MatingNode {
    MatingNode::create(
        ctx,
        MatingNode {
            id: <HeerId as PrimaryKey>::sentinel(),
            created_at: DateTime::UNIX_EPOCH,
            updated_at: DateTime::UNIX_EPOCH,
            label: label.to_string(),
            mother_id: mother.map(|m| ForeignKey::new(m.id)),
            father_id: father.map(|f| ForeignKey::new(f.id)),
        },
    )
    .await
    .expect("seed mating node")
}

/// Run the typed pair-tuple closure self-join and pluck the F value for
/// a specific `(left, right)` pair. Panics if the pair is absent — the
/// caller has already established that both sides are in the candidate
/// id-list, so a missing row is a substrate regression worth surfacing
/// as a panic instead of a soft None.
async fn kinship_f_for_pair(
    ctx: &mut DjogiContext,
    left_id: HeerId,
    right_id: HeerId,
    candidate_left_ids: &[HeerId],
    candidate_right_ids: &[HeerId],
) -> f64 {
    let pairs: Vec<((MatingNode, MatingNode), f64)> = MatingNode::objects()
        .self_pairs()
        .filter_left(|l| l.id().in_(candidate_left_ids.to_vec()))
        .filter_right(|r| r.id().in_(candidate_right_ids.to_vec()))
        .left_join_closure_pair::<MatingAncestry>()
        .annotate(|_l, _r| PairClosureKinshipSum::<MatingAncestry>::new())
        .fetch_all(ctx)
        .await
        .expect("typed pair-tuple closure self-join");

    pairs
        .iter()
        .find(|((l, r), _)| l.id == left_id && r.id == right_id)
        .map(|(_, f)| *f)
        .unwrap_or_else(|| {
            panic!(
                "no pair-tuple row for (left={:?}, right={:?}) — substrate \
                 regression in JoinedQuerySet::fetch_all (closure-pair branch). \
                 Found {} pairs total.",
                left_id.as_i64(),
                right_id.as_i64(),
                pairs.len(),
            )
        })
}

const F_TOLERANCE: f64 = 1e-9;

fn assert_f_close(actual: f64, expected: f64, label: &str) {
    assert!(
        (actual - expected).abs() < F_TOLERANCE,
        "{label}: expected F = {expected}, got {actual} (delta = {})",
        actual - expected,
    );
}

// ── 1. Unrelated parents — F = 0 ───────────────────────────────────────────

#[djogi::djogi_test(sync_models = [MatingNode, MatingAncestry])]
async fn wright_f_unrelated_is_zero(mut ctx: djogi::DjogiContext) {
    // Two orphan nodes with no common ancestor.
    let a = seed(&mut ctx, "a", None, None).await;
    let b = seed(&mut ctx, "b", None, None).await;

    MatingNode::materialize_closure::<MatingAncestry>(
        &mut ctx,
        djogi::query::MaterializeClosureOptions::default(),
    )
    .await
    .expect("materialize closure");

    let f = kinship_f_for_pair(&mut ctx, a.id, b.id, &[a.id], &[b.id]).await;
    assert_f_close(
        f,
        0.0,
        "unrelated parents must yield F = 0 (PairClosureKinshipSum sums over \
         common ancestors; there are none, so SUM is 0 and COALESCE keeps it 0)",
    );
}

// ── 2. Parent × child — F = 0.25 ───────────────────────────────────────────

#[djogi::djogi_test(sync_models = [MatingNode, MatingAncestry])]
async fn wright_f_parent_child_is_one_quarter(mut ctx: djogi::DjogiContext) {
    // Parent A; child B is offspring of A (mother edge). If we were to
    // mate A and B, the offspring would inherit half its DNA from A on
    // each parental side — F = 0.25. The closure represents A as an
    // ancestor of B at depth 1, and A is its own ancestor at depth 0,
    // so the Wright sum picks A with d_A = 0, d_B = 1: term =
    // 0.5^(0+1+1) = 1/4.
    let parent = seed(&mut ctx, "parent", None, None).await;
    let child = seed(&mut ctx, "child", Some(&parent), None).await;

    MatingNode::materialize_closure::<MatingAncestry>(
        &mut ctx,
        djogi::query::MaterializeClosureOptions::default(),
    )
    .await
    .expect("materialize closure");

    let f = kinship_f_for_pair(&mut ctx, parent.id, child.id, &[parent.id], &[child.id]).await;
    assert_f_close(f, 0.25, "parent × child must yield F = 0.25 (term = 1/4)");
}

// ── 3. Full siblings — F = 0.25 ────────────────────────────────────────────

#[djogi::djogi_test(sync_models = [MatingNode, MatingAncestry])]
async fn wright_f_full_siblings_is_one_quarter(mut ctx: djogi::DjogiContext) {
    // Parents mom, dad. Children sib_a and sib_b share both parents.
    // Common ancestors of (sib_a, sib_b): mom (d=1, d=1) and dad
    // (d=1, d=1). Per ancestor: 0.5^(1+1+1) = 1/8. Sum = 2/8 = 1/4.
    let mom = seed(&mut ctx, "mom", None, None).await;
    let dad = seed(&mut ctx, "dad", None, None).await;
    let sib_a = seed(&mut ctx, "sib_a", Some(&mom), Some(&dad)).await;
    let sib_b = seed(&mut ctx, "sib_b", Some(&mom), Some(&dad)).await;

    MatingNode::materialize_closure::<MatingAncestry>(
        &mut ctx,
        djogi::query::MaterializeClosureOptions::default(),
    )
    .await
    .expect("materialize closure");

    let f = kinship_f_for_pair(&mut ctx, sib_a.id, sib_b.id, &[sib_a.id], &[sib_b.id]).await;
    assert_f_close(
        f,
        0.25,
        "full siblings must yield F = 0.25 (two shared ancestors at d=1,1 \
         contributing 1/8 each)",
    );
}

// ── 4. Half siblings — F = 0.125 ───────────────────────────────────────────

#[djogi::djogi_test(sync_models = [MatingNode, MatingAncestry])]
async fn wright_f_half_siblings_is_one_eighth(mut ctx: djogi::DjogiContext) {
    // Single shared mother; fathers different (one None, one provided).
    // Common ancestor: mom (d=1, d=1). Term = 0.5^(1+1+1) = 1/8.
    let mom = seed(&mut ctx, "mom", None, None).await;
    let dad_a = seed(&mut ctx, "dad_a", None, None).await;
    let dad_b = seed(&mut ctx, "dad_b", None, None).await;
    let half_a = seed(&mut ctx, "half_a", Some(&mom), Some(&dad_a)).await;
    let half_b = seed(&mut ctx, "half_b", Some(&mom), Some(&dad_b)).await;

    MatingNode::materialize_closure::<MatingAncestry>(
        &mut ctx,
        djogi::query::MaterializeClosureOptions::default(),
    )
    .await
    .expect("materialize closure");

    let f = kinship_f_for_pair(&mut ctx, half_a.id, half_b.id, &[half_a.id], &[half_b.id]).await;
    assert_f_close(
        f,
        0.125,
        "half siblings (shared mother only) must yield F = 0.125 (single \
         shared ancestor at d=1,1 → 1/8)",
    );
}

// ── 5. First cousins — F = 0.0625 ──────────────────────────────────────────

#[djogi::djogi_test(sync_models = [MatingNode, MatingAncestry])]
async fn wright_f_first_cousins_is_one_sixteenth(mut ctx: djogi::DjogiContext) {
    // Grandparents g_mom, g_dad. Their two children m_uncle and m_aunt
    // are full siblings. Each marries an unrelated partner and has one
    // child: cousin_a (parents m_uncle + spouse_a) and cousin_b
    // (parents m_aunt + spouse_b). Common ancestors of cousin_a and
    // cousin_b: g_mom (d=2, d=2) and g_dad (d=2, d=2). Per ancestor:
    // 0.5^(2+2+1) = 1/32. Sum = 2/32 = 1/16 = 0.0625.
    let g_mom = seed(&mut ctx, "g_mom", None, None).await;
    let g_dad = seed(&mut ctx, "g_dad", None, None).await;
    let m_uncle = seed(&mut ctx, "m_uncle", Some(&g_mom), Some(&g_dad)).await;
    let m_aunt = seed(&mut ctx, "m_aunt", Some(&g_mom), Some(&g_dad)).await;
    let spouse_a = seed(&mut ctx, "spouse_a", None, None).await;
    let spouse_b = seed(&mut ctx, "spouse_b", None, None).await;
    let cousin_a = seed(&mut ctx, "cousin_a", Some(&spouse_a), Some(&m_uncle)).await;
    let cousin_b = seed(&mut ctx, "cousin_b", Some(&spouse_b), Some(&m_aunt)).await;

    MatingNode::materialize_closure::<MatingAncestry>(
        &mut ctx,
        djogi::query::MaterializeClosureOptions::default(),
    )
    .await
    .expect("materialize closure");

    let f = kinship_f_for_pair(
        &mut ctx,
        cousin_a.id,
        cousin_b.id,
        &[cousin_a.id],
        &[cousin_b.id],
    )
    .await;
    assert_f_close(
        f,
        0.0625,
        "first cousins must yield F = 1/16 = 0.0625 (two shared grandparents \
         at d=2,2 → 1/32 each)",
    );
}

// ── 6. Multi-path multiplicity — F doubles when a common ancestor is reached
//     through two distinct edge sequences ───────────────────────────────────

#[djogi::djogi_test(sync_models = [MatingNode, MatingAncestry])]
async fn wright_f_path_multiplicity_doubles_term(mut ctx: djogi::DjogiContext) {
    // Linebreeding: common ancestor C is reachable from each parent
    // through TWO distinct edge sequences (mother-mother and father-
    // father). `materialize_closure` records this as path_count = 2
    // for the (parent, C, depth=2) closure row. The Wright sum picks
    // up the multiplicity:
    //   term = path_count_left * path_count_right * 0.5^(d_l + d_r + 1)
    //        = 2 * 2 * 0.5^(2+2+1)
    //        = 4 * 1/32
    //        = 1/8
    //
    // Pedigree shape:
    //                 common (C)
    //                /          \
    //              mom            dad
    //             /  \           /  \
    //           a_m   a_f       b_m  b_f
    //            ^     ^         ^     ^
    //         A's mom  A's dad  B's mom B's dad   (NOT — see below)
    //
    // Simpler version: A's mother and A's father are both children of
    // C (full siblings); A is thus the offspring of a full-sibling
    // incest pairing. Common ancestors of A: mother (d=1), father
    // (d=1), C reached via mother-mother (d=2) AND C reached via
    // father-father (d=2). path_count for (A, C, 2) is 2.
    //
    // For the pair (A, B) where B is reached the same way through C
    // (mother-father siblings, both descended from C), the closure
    // records (A, C, 2, path_count=2) and (B, C, 2, path_count=2).
    // PairClosureKinshipSum joins on ancestor_id, so the SUM picks up
    // 2 * 2 * 1/32 = 1/8 for C — plus whatever the mom/dad rows
    // contribute below.
    let common = seed(&mut ctx, "common", None, None).await;
    let a_mom = seed(&mut ctx, "a_mom", Some(&common), Some(&common)).await;
    let a_dad = seed(&mut ctx, "a_dad", Some(&common), Some(&common)).await;
    let b_mom = seed(&mut ctx, "b_mom", Some(&common), Some(&common)).await;
    let b_dad = seed(&mut ctx, "b_dad", Some(&common), Some(&common)).await;
    let node_a = seed(&mut ctx, "node_a", Some(&a_mom), Some(&a_dad)).await;
    let node_b = seed(&mut ctx, "node_b", Some(&b_mom), Some(&b_dad)).await;

    MatingNode::materialize_closure::<MatingAncestry>(
        &mut ctx,
        djogi::query::MaterializeClosureOptions::default(),
    )
    .await
    .expect("materialize closure");

    let f = kinship_f_for_pair(&mut ctx, node_a.id, node_b.id, &[node_a.id], &[node_b.id]).await;

    // This test pins multiplicity behaviour — the exact reference value
    // is documented by computing F manually from the closure rows.
    //
    // (node_a, common):
    //   - mother-mother:  d=2 via a_mom → common (mother edge of common)
    //   - mother-father:  d=2 via a_mom → common (father edge of common)
    //   - father-mother:  d=2 via a_dad → common (mother edge of common)
    //   - father-father:  d=2 via a_dad → common (father edge of common)
    //   So path_count(node_a, common, d=2) = 4.
    //
    // Symmetrically, path_count(node_b, common, d=2) = 4.
    //
    // PairClosureKinshipSum term for ancestor `common`:
    //   4 * 4 * 0.5^(2+2+1) = 16 * 1/32 = 0.5
    //
    // (a_mom is NOT an ancestor of node_b, and b_mom is NOT an
    // ancestor of node_a — the only shared ancestor is `common`.)
    //
    // So expected F = 0.5.
    let expected = 0.5;
    assert_f_close(
        f,
        expected,
        "linebreeding via shared great-grandparent must surface path-count \
         multiplicity (4 × 4 × 0.5^5 = 0.5)",
    );
}
