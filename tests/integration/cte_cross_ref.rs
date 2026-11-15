// djogi#483 — cross-CTE column reference in a consumer WHERE predicate,
// against a live Postgres + PostGIS.
//
// Shape under test:
//   WITH anchor AS (SELECT id, geo FROM places WHERE id = $1),
//        places_cte AS (SELECT ... FROM places)
//   SELECT <place columns> FROM places_cte, anchor
//   WHERE ST_DWithin(places_cte.geo, anchor.geo, 50000)
//
// Delivered via from_cte("places_cte") + also_from_cte("anchor") (which puts
// the sibling `anchor` term in the consumer FROM so its columns resolve) + a
// CteRef("anchor","geo") cross-reference. `anchor` is a single-row lookup, so
// `FROM places_cte, anchor` is a 1×N implicit cross join (the intended
// correlation), not a full cartesian product. Column qualification is
// automatic when also_from is non-empty — the SELECT list, WHERE predicate,
// and ORDER BY all qualify to the `places_cte` term.

#![cfg(feature = "spatial")]

use djogi::geo::GeoPoint;
use djogi::prelude::*;

#[model(table = "ccr_places", pk = HeerIdRecencyBiased, no_default)]
#[derive(Debug, Clone)]
pub struct CcrPlace {
    pub name: String,
    pub geo: GeoPoint,
}

fn ccr_place(name: &str, lat: f64, lon: f64) -> CcrPlace {
    CcrPlace {
        id: djogi::HeerIdDesc::from_i64(0).expect("HeerIdDesc sentinel"),
        created_at: djogi::DateTime::UNIX_EPOCH,
        updated_at: djogi::DateTime::UNIX_EPOCH,
        name: name.into(),
        geo: GeoPoint::new(lat, lon).expect("valid coordinate"),
    }
}

#[djogi::djogi_test(extensions = ["postgis"], sync_models = [CcrPlace])]
async fn cross_cte_dwithin_filters_by_anchor_geo(mut ctx: djogi::DjogiContext) {
    // Seed: an anchor place and two others — one within 50km, one far.
    let anchor = CcrPlace::create(&mut ctx, ccr_place("anchor", 0.0, 0.0))
        .await
        .expect("create anchor");
    let near = CcrPlace::create(&mut ctx, ccr_place("near", 0.1, 0.1)) // ~15 km
        .await
        .expect("create near");
    let _far = CcrPlace::create(&mut ctx, ccr_place("far", 5.0, 5.0)) // ~780 km
        .await
        .expect("create far");

    // WITH anchor AS (SELECT ... FROM places WHERE id = <anchor.id>)
    // SELECT ... FROM places_cte, anchor
    // WHERE ST_DWithin(places_cte.geo, anchor.geo, 50000)
    let results = CcrPlace::objects()
        .with(
            "anchor",
            CcrPlace::objects().filter(|p| p.id().eq(*anchor.pk_value())),
        )
        .expect("with anchor term")
        .with("places_cte", CcrPlace::objects())
        .expect("with places_cte term")
        .from_cte("places_cte")
        .expect("from_cte")
        // Bring the sibling `anchor` CTE into the consumer FROM so the
        // CteRef("anchor","geo") predicate resolves (Postgres requires the
        // term in FROM). `anchor` is a single-row lookup, so
        // `FROM places_cte, anchor` is a 1×N correlation. Column qualification
        // (SELECT / WHERE / ORDER BY) is automatic while also_from is non-empty.
        .also_from_cte("anchor")
        .expect("also_from_cte anchor")
        .filter_expr(|p| {
            Expr::dwithin(
                p.geo().as_expr(),
                CteRef::new("anchor", "geo")
                    .expect("valid cte ref")
                    .as_expr::<GeoPoint>(),
                50_000.0,
            )
        })
        .fetch_all(&mut ctx)
        .await
        .expect("cross-cte dwithin query");

    // anchor (distance 0) and near (~15 km) are within 50 km; far is not.
    let names: Vec<&str> = results.iter().map(|p| p.name.as_str()).collect();
    assert!(
        names.contains(&"anchor"),
        "anchor within 50km of itself: {names:?}"
    );
    assert!(names.contains(&"near"), "near within 50km: {names:?}");
    assert!(!names.contains(&"far"), "far must be excluded: {names:?}");

    // Path B — spatial ORDER BY under also_from_cte must also qualify both the
    // ST_Distance term and the PK tiebreak to the `places_cte` term, so the
    // two-term consumer FROM does not raise a Postgres ambiguity error. Order
    // by distance ascending from the anchor's coordinate.
    let center = GeoPoint::new(0.0, 0.0).expect("valid center");
    let ordered = CcrPlace::objects()
        .with(
            "anchor",
            CcrPlace::objects().filter(|p| p.id().eq(*anchor.pk_value())),
        )
        .expect("with anchor term")
        .with("places_cte", CcrPlace::objects())
        .expect("with places_cte term")
        .from_cte("places_cte")
        .expect("from_cte")
        .also_from_cte("anchor")
        .expect("also_from_cte anchor")
        .filter_expr(|p| {
            Expr::dwithin(
                p.geo().as_expr(),
                CteRef::new("anchor", "geo")
                    .expect("valid cte ref")
                    .as_expr::<GeoPoint>(),
                50_000.0,
            )
        })
        .order_by(|p| p.geo().order_by_distance(center))
        .fetch_all(&mut ctx)
        .await
        .expect("cross-cte dwithin ordered query");

    // anchor (distance 0) comes before near (~15 km); far is still excluded.
    let ordered_names: Vec<&str> = ordered.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        ordered_names,
        vec!["anchor", "near"],
        "ascending-distance order expected: {ordered_names:?}"
    );

    let _ = near;
}
