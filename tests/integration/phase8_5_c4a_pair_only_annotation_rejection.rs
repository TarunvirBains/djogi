// Phase 8.5 Cluster A follow-up — pair-only annotation rejection on the
// single-Model + grouped annotate paths.
//
// # What this fixture pins
//
// GPT-5.5 xhigh BLOCK (cluster-A follow-up, finding #2): the
// `PairAreaOverlapRatio<L, R>` annotation slot reports `is_joined_safe()
// = true` (its emitted SQL alias-qualifies both column references
// explicitly), but until this slice was added it did **not** override
// the `requires_pair_tuple_scope()` signal. The single-Model
// `QuerySet::annotate(...)` and the grouped annotate terminals only
// rejected slots whose `requires_closure_pair_join()` returned true
// (today: `PairClosureKinshipSum<C>`), so a user-side
// `Mini::objects().annotate(|_| PairAreaOverlapRatio::new(...))` chain
// would compile and surface as a Postgres
// `42P01 missing FROM-clause entry for table "l"` error at execute
// time. The fix splits the two signals — `requires_closure_pair_join()`
// stays for closure-pair-only slots; `requires_pair_tuple_scope()` is
// the broader pair-tuple `l.`/`r.` invariant. Both
// `PairAreaOverlapRatio` and `PairClosureKinshipSum` override the new
// signal; both single-Model and grouped paths consult both.
//
// This fixture exercises the typed validation error directly:
//
//   1. `QuerySet::<Mini>::new().annotate(PairAreaOverlapRatio).fetch_all`
//      MUST return `DjogiError::Validation` mentioning
//      `PairAreaOverlapRatio` and the `self_pairs()` remediation.
//   2. Same for `QuerySet::<Mini>::new().annotate(PairClosureKinshipSum)
//      .fetch_all` — historically rejected via the narrower closure-pair
//      signal, must remain rejected after the broader scope refactor.
//   3. Grouped path: `Mini::objects().group_by(|f| (f.id(),))
//      .annotate(|_| PairAreaOverlapRatio).fetch_all` MUST return the
//      same validation error shape.
//
// The gate fires BEFORE any DB interaction (`AnnotatedQuerySet::fetch_all`
// returns immediately when the queryset is non-empty and the aggregates
// trip the scope check), so this test does not require a populated
// Postgres but the `#[djogi_test]` harness needs a database for
// connection-bound type-checking. The `Mini` model is the minimal shape
// the test needs: a single i32 column for grouping.

#![cfg(feature = "spatial")]

use djogi::DjogiError;
use djogi::prelude::*;
use djogi::query::{PairAreaOverlapRatio, PairClosureKinshipSum};

// ── Models ──────────────────────────────────────────────────────────

#[model(table = "phase8_5_c4a_rejected_minis", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Mini {
    pub label: String,
    pub category: i32,
    /// Geography column used for the overlap-ratio constructor's
    /// `SpatialColumnValue` bound — the rejection gate fires before the
    /// SQL is built so the column never reaches Postgres in this test,
    /// but the constructor still needs a column type that satisfies
    /// the spatial trait.
    pub territory: Option<djogi::geo::Polygon>,
}

#[model(
    table = "phase8_5_c4a_rejected_mini_ancestries",
    pk = HeerId,
    no_default,
    indexes(unique(fields = [node_id, ancestor_id, depth]))
)]
#[derive(Debug, Clone)]
pub struct MiniAncestry {
    pub node_id: ForeignKey<Mini>,
    pub ancestor_id: ForeignKey<Mini>,
    pub depth: i32,
    pub path_count: i64,
}

impl djogi::query::ClosureModel for MiniAncestry {
    type Source = Mini;
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

// ── Helpers ─────────────────────────────────────────────────────────

async fn seed_one(ctx: &mut djogi::DjogiContext) {
    // Need ≥ 1 row so `qs.is_empty()` is false at the terminal; the
    // gate has a short-circuit branch for empty querysets that bypasses
    // the validation check. With one row, the scope gate is the first
    // branch the terminal hits.
    Mini::create(
        ctx,
        Mini {
            id: <HeerId as PrimaryKey>::sentinel(),
            created_at: DateTime::UNIX_EPOCH,
            updated_at: DateTime::UNIX_EPOCH,
            label: "a".to_string(),
            category: 1,
            territory: None,
        },
    )
    .await
    .expect("seed mini row");
}

/// Assert the validation error carries the expected slot-name +
/// remediation hint. The message is shared across the single-Model and
/// grouped paths so the assertion can be DRY across all rejection
/// tests.
fn assert_pair_only_rejection(err: DjogiError) {
    match err {
        DjogiError::Validation(msg) => {
            assert!(
                msg.contains("pair-tuple aggregate"),
                "rejection message should call out the pair-tuple invariant — got: {msg}",
            );
            assert!(
                msg.contains("self_pairs()"),
                "rejection message should point at the `.self_pairs()` remediation — got: {msg}",
            );
        }
        other => panic!("expected DjogiError::Validation, got: {other:?}"),
    }
}

// ── 1. PairAreaOverlapRatio on single-Model QuerySet::annotate ──────

#[djogi::djogi_test(
    extensions = ["postgis"],
    sync_models = [Mini],
)]
async fn pair_area_overlap_ratio_rejected_on_single_model_annotate(mut ctx: djogi::DjogiContext) {
    seed_one(&mut ctx).await;
    let err = Mini::objects()
        .annotate(|f| PairAreaOverlapRatio::<Mini, Mini>::new(f.territory(), f.territory()))
        .fetch_all(&mut ctx)
        .await
        .expect_err(
            "PairAreaOverlapRatio on single-Model annotate must surface DjogiError::Validation",
        );
    assert_pair_only_rejection(err);
}

// ── 2. PairClosureKinshipSum on single-Model QuerySet::annotate ─────
//
// Historical rejection path (via `requires_closure_pair_join()`) — must
// remain rejected after the broader scope refactor. The new gate is the
// boolean OR of both signals.

#[djogi::djogi_test(
    extensions = ["postgis"],
    sync_models = [Mini, MiniAncestry],
)]
async fn pair_closure_kinship_sum_rejected_on_single_model_annotate(mut ctx: djogi::DjogiContext) {
    seed_one(&mut ctx).await;
    let err = Mini::objects()
        .annotate(|_| PairClosureKinshipSum::<MiniAncestry>::new())
        .fetch_all(&mut ctx)
        .await
        .expect_err(
            "PairClosureKinshipSum on single-Model annotate must surface DjogiError::Validation",
        );
    assert_pair_only_rejection(err);
}

// ── 3. PairAreaOverlapRatio on grouped single-Model annotate ────────
//
// `QuerySet::group_by(...)` returns a `GroupedQuerySet<T, K>` whose
// `annotate(...)` produces a `GroupedAnnotatedQuerySet`. The terminal
// `.fetch_all` consults the same `requires_pair_tuple_scope()` signal
// and must reject the pair-only slot identically.
//
// `group_by` admits a bare `DjogiField<M, V>` for arity-1 key shape;
// single-element tuples (`(f.category(),)`) are deliberately not in
// the `IntoGroupKeyTuple` impl set so the key shape stays unambiguous
// between "one key" and "a tuple of keys".

#[djogi::djogi_test(
    extensions = ["postgis"],
    sync_models = [Mini],
)]
async fn pair_area_overlap_ratio_rejected_on_grouped_annotate(mut ctx: djogi::DjogiContext) {
    seed_one(&mut ctx).await;
    let err = Mini::objects()
        .group_by(|f| f.category())
        .annotate(|f| PairAreaOverlapRatio::<Mini, Mini>::new(f.territory(), f.territory()))
        .fetch_all(&mut ctx)
        .await
        .expect_err("PairAreaOverlapRatio on grouped annotate must surface DjogiError::Validation");
    assert_pair_only_rejection(err);
}
