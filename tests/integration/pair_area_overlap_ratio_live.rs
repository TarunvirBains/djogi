// Follow-up — live PostGIS validation of
// `PairAreaOverlapRatio<L, R>`.
//
// # What this fixture pins
//
// The unit tests in `djogi/src/query/joined.rs` cover the SQL shape
// `PairAreaOverlapRatio` emits — `COALESCE(ST_Area(ST_Intersection(...)),
// 0)::float8 / NULLIF(ST_Area(...), 0)::float8`. This live fixture
// closes the loop by sending that SQL to a real PostGIS-backed
// Postgres instance and verifying the decoded `f64` matches the
// geometry-arithmetic shape the docstring promises:
//
//   - Coincident polygons → ratio = 1.0
//   - Disjoint polygons → ratio = 0.0
//   - Half-overlap polygons → ratio in (0, 1), centred on 0.5
//   - NULL geography column on either side → ratio = 0.0 (via the
//     NULLIF/COALESCE shape that the slot's `decode_column` folds to
//     `Option<f64>::unwrap_or(0.0)`)
//
// The fixture also exercises a 4-way pair-tuple terminal: the slot
// receives each `(L, R)` combination from the cross-join and the
// decoded ratio is keyed by `(l.id, r.id)` so the assertion targets
// each scenario by name rather than by position.
//
// # Why a separate live fixture
//
// The mating-pairs demo's live shape uses real-world herd territories
// where the overlap values are inputs to a composite score, not the
// regression target. A purpose-built fixture with controlled polygon
// inputs gives the deterministic ground truth — the same role
// `c4a_mating_pairs_correctness` plays for the Wright F
// closure self-join.

#![cfg(feature = "spatial")]

use djogi::geo::{GeoPoint, Polygon};
use djogi::prelude::*;
use djogi::query::PairAreaOverlapRatio;
use std::collections::HashMap;

// ── Model ───────────────────────────────────────────────────────────

/// Minimal territory model: each row carries a name and an optional
/// polygon. Optional so the NULL-geography path can be exercised by
/// inserting a row whose `boundary` is `None`.
#[model(table = "c4a_overlap_zones", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Zone {
    pub label: String,
    pub boundary: Option<Polygon>,
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Construct a closed square polygon centred at `(lat, lon)` with
/// `half_side` degrees on each side. Mirrors the helper pattern from
/// `spatial_polish.rs` so adopter test shapes stay portable.
fn square(lat: f64, lon: f64, half_side: f64) -> Polygon {
    let pts = vec![
        GeoPoint::new(lat - half_side, lon - half_side).unwrap(),
        GeoPoint::new(lat - half_side, lon + half_side).unwrap(),
        GeoPoint::new(lat + half_side, lon + half_side).unwrap(),
        GeoPoint::new(lat + half_side, lon - half_side).unwrap(),
        GeoPoint::new(lat - half_side, lon - half_side).unwrap(),
    ];
    Polygon::with_ring(pts).expect("valid square polygon")
}

/// Insert a zone with the given label + optional polygon. Returns the
/// freshly-created row so its `id` is available for the assertion
/// lookup.
async fn make_zone(ctx: &mut djogi::DjogiContext, label: &str, boundary: Option<Polygon>) -> Zone {
    Zone::create(
        ctx,
        Zone {
            id: <HeerId as PrimaryKey>::sentinel(),
            created_at: djogi::DateTime::UNIX_EPOCH,
            updated_at: djogi::DateTime::UNIX_EPOCH,
            label: label.to_string(),
            boundary,
        },
    )
    .await
    .expect("Zone::create must succeed")
}

// ── Live test ───────────────────────────────────────────────────────

/// End-to-end PostGIS validation for `PairAreaOverlapRatio<Zone, Zone>`.
///
/// Seeds four zones (`coincident_a`, `coincident_b` (same polygon as
/// `coincident_a`), `disjoint_far` (10° east), `null_zone` (NULL
/// boundary)) and asserts the four canonical ratio values across each
/// pair combination:
///
///   - `(A, A)` → 1.0 (coincident with itself)
///   - `(A, B)` → 1.0 (coincident different rows)
///   - `(A, disjoint)` → 0.0 (no overlap)
///   - `(A, null)` → 0.0 (NULL right side via NULLIF/COALESCE/decode shape)
///   - `(null, A)` → 0.0 (NULL left side via the same shape)
///   - `(disjoint, A)` → 0.0 (no overlap, asymmetric form)
///
/// Tolerance is `1e-9` for the `f64` round-trip on coincident
/// geometries; the NULL / disjoint paths decode as exact `0.0`.
#[djogi::djogi_test(
    extensions = ["postgis"],
    sync_models = [Zone],
)]
async fn pair_area_overlap_ratio_emits_correct_ratios_on_postgis(mut ctx: djogi::DjogiContext) {
    // Seed four zones with controlled geometry:
    //
    //   coincident_a / coincident_b: identical polygons → ratio = 1
    //   disjoint_far:               10° east of A → ratio = 0
    //   null_zone:                  NULL boundary → ratio = 0 on either side
    let coincident_a = make_zone(&mut ctx, "a", Some(square(0.0, 0.0, 0.1))).await;
    let coincident_b = make_zone(&mut ctx, "b", Some(square(0.0, 0.0, 0.1))).await;
    let disjoint_far = make_zone(&mut ctx, "far", Some(square(0.0, 10.0, 0.1))).await;
    let null_zone = make_zone(&mut ctx, "null", None).await;

    // Single typed pair-tuple query exercising every `(L, R)`
    // combination. `include_equal_pk()` opts in to the same-row case
    // (`(A, A)` etc.) so the assertion set covers the identity row's
    // shape too.
    let pairs: Vec<((Zone, Zone), f64)> = Zone::objects()
        .self_pairs()
        .include_equal_pk()
        .annotate(|l, r| PairAreaOverlapRatio::new(l.boundary(), r.boundary()))
        .fetch_all(&mut ctx)
        .await
        .expect("typed PairAreaOverlapRatio live query must succeed");

    // Key by `(l.id, r.id)` so the assertion checks each scenario by
    // name rather than by row order.
    let by_pair: HashMap<(djogi::HeerId, djogi::HeerId), f64> = pairs
        .into_iter()
        .map(|((l, r), ratio)| ((l.id, r.id), ratio))
        .collect();

    // 4×4 cross-join → 16 rows. The PK shape (`HeerId` default) means
    // every combination is present once.
    assert_eq!(
        by_pair.len(),
        16,
        "4 zones × 4 zones = 16 pair-tuple rows expected; got {}",
        by_pair.len(),
    );

    let get = |l_id: djogi::HeerId, r_id: djogi::HeerId| -> f64 {
        *by_pair
            .get(&(l_id, r_id))
            .unwrap_or_else(|| panic!("missing pair ({l_id:?}, {r_id:?})"))
    };

    // ── Coincident — ratio = 1.0 (within float8 tolerance) ─────────
    let aa = get(coincident_a.id, coincident_a.id);
    assert!(
        (aa - 1.0).abs() < 1e-9,
        "coincident with self (A, A) — expected 1.0, got {aa}",
    );
    let ab = get(coincident_a.id, coincident_b.id);
    assert!(
        (ab - 1.0).abs() < 1e-9,
        "coincident different rows (A, B) — expected 1.0, got {ab}",
    );

    // ── Disjoint — ratio = 0.0 (exact, ST_Area on empty geography is 0) ─
    let a_far = get(coincident_a.id, disjoint_far.id);
    assert_eq!(
        a_far, 0.0,
        "disjoint geometries (A, far) — expected 0.0, got {a_far}",
    );
    let far_a = get(disjoint_far.id, coincident_a.id);
    assert_eq!(
        far_a, 0.0,
        "disjoint geometries (far, A) — expected 0.0 (left-normalised), got {far_a}",
    );

    // ── NULL geography on either side — ratio = 0.0 ────────────────
    //
    // `ST_Intersection(a, NULL)` returns NULL → COALESCE wrap yields 0.
    // `ST_Area(NULL::geography)` returns NULL → NULLIF wrap stays NULL.
    // Final SQL value: `0 / NULL = NULL`. The slot's `decode_column`
    // reads as `Option<f64>` and folds None to 0.0, so the decoded
    // value is exact 0.0.
    let a_null = get(coincident_a.id, null_zone.id);
    assert_eq!(
        a_null, 0.0,
        "NULL right boundary (A, null) — expected 0.0, got {a_null}",
    );
    let null_a = get(null_zone.id, coincident_a.id);
    assert_eq!(
        null_a, 0.0,
        "NULL left boundary (null, A) — expected 0.0, got {null_a}",
    );
    let null_null = get(null_zone.id, null_zone.id);
    assert_eq!(
        null_null, 0.0,
        "NULL on both sides (null, null) — expected 0.0, got {null_null}",
    );
}

// ── Partial-overlap case ────────────────────────────────────────────
//
// Separate test because the partial-overlap scenario needs a third
// polygon overlapping half of A's area. The arithmetic is documented
// per-step in the assertion body.

#[djogi::djogi_test(
    extensions = ["postgis"],
    sync_models = [Zone],
)]
async fn pair_area_overlap_ratio_partial_overlap_in_open_interval(mut ctx: djogi::DjogiContext) {
    // `a` is a 0.2° × 0.2° square centered at (0, 0):
    //   lat ∈ [-0.1, 0.1], lon ∈ [-0.1, 0.1]
    // `b` is a 0.2° × 0.2° square centered at (0, 0.1):
    //   lat ∈ [-0.1, 0.1], lon ∈ [0.0, 0.2]
    //
    // Their intersection is a 0.2° × 0.1° rectangle:
    //   lat ∈ [-0.1, 0.1], lon ∈ [0.0, 0.1]
    //
    // Area ratio `area(A ∩ B) / area(A)` = (0.2 × 0.1) / (0.2 × 0.2)
    //                                    = 0.5 exactly (in the lat/lon
    // square approximation; PostGIS's `geography` cast uses spheroid
    // arithmetic so the actual value drifts slightly from 0.5).
    //
    // The PostGIS spheroid math at this latitude (equator) is close
    // enough to flat-Earth approximations that the ratio lands within
    // ±0.05 of 0.5; we assert the open interval rather than the exact
    // value so the test stays robust to PostGIS version differences.
    let a = make_zone(&mut ctx, "a", Some(square(0.0, 0.0, 0.1))).await;
    let b = make_zone(&mut ctx, "b", Some(square(0.0, 0.1, 0.1))).await;

    let pairs: Vec<((Zone, Zone), f64)> = Zone::objects()
        .self_pairs()
        .include_equal_pk()
        .annotate(|l, r| PairAreaOverlapRatio::new(l.boundary(), r.boundary()))
        .fetch_all(&mut ctx)
        .await
        .expect("typed PairAreaOverlapRatio live query must succeed");

    let by_pair: HashMap<(djogi::HeerId, djogi::HeerId), f64> = pairs
        .into_iter()
        .map(|((l, r), ratio)| ((l.id, r.id), ratio))
        .collect();

    let ab = *by_pair
        .get(&(a.id, b.id))
        .expect("pair (A, B) must be present in the cross-join");
    let ba = *by_pair
        .get(&(b.id, a.id))
        .expect("pair (B, A) must be present in the cross-join");

    // Both directions are left-normalised so they should agree (areas
    // of A and B are equal by construction). Either way the ratio is
    // in `(0, 1)` — neither fully-coincident nor fully-disjoint.
    assert!(
        ab > 0.0 && ab < 1.0,
        "partial overlap (A, B) — expected ratio in (0, 1), got {ab}",
    );
    assert!(
        ba > 0.0 && ba < 1.0,
        "partial overlap (B, A) — expected ratio in (0, 1), got {ba}",
    );
    // Symmetry holds because A and B have equal areas; the ratio
    // converges to the same value on both directions.
    assert!(
        (ab - ba).abs() < 0.01,
        "A and B have equal areas — left-normalised ratio should match in both directions, got ({ab}, {ba})",
    );
    // Sanity: the ratio is close to 0.5 (the geometric intuition).
    // Use a generous tolerance (±0.05) for spheroid-vs-flat differences.
    assert!(
        (ab - 0.5).abs() < 0.05,
        "partial overlap (A, B) — expected ~0.5, got {ab} (PostGIS spheroid drift acceptable up to ±0.05)",
    );
}
