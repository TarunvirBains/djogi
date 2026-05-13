// Phase 7-Zero-2 polish plus Phase 8eta routing — `Option<GeoPoint>`
// columns keep the SQL-only `.within_km` route through
// `.explicit_pg_predicate()` and keep `.order_by_distance` on the root
// field handle.
//
// Pre-this-change, the spatial methods lived only on
// `FieldRef<M, GeoPoint>`, so models with nullable location columns
// were forced to either drop the `Option` wrapper (and use a sentinel
// like `GeoPoint::new(0.0, 0.0)` meaning "unknown" — the kind of
// schema contortion the lens explicitly rejects) or fall back to raw
// `ST_DWithin` with hand-written `IS NOT NULL` guards. The polish
// lifts the methods onto the nullable SQL-only variant so callers can
// swap `GeoPoint` for `Option<GeoPoint>` at the schema level while
// preserving the explicit PostGIS predicate route.
//
// The fixture is gated on `#[cfg(feature = "spatial")]` to mirror the
// pattern in `phase6_spatial_field.rs`: under default features the
// fixture compiles trivially as `fn main() {}`; under
// `--features spatial` the full surface is exercised. Both runs ship
// lihaaf-clean.

#[cfg(feature = "spatial")]
use djogi::prelude::*;

#[cfg(feature = "spatial")]
#[allow(dead_code)]
#[model(table = "phase7_zero2_option_geopoint_places", no_default)]
#[derive(Debug, Clone)]
pub struct Place {
    pub name: String,
    pub location: Option<djogi::GeoPoint>,
}

#[cfg(feature = "spatial")]
#[allow(dead_code)]
fn _within_km_compiles_on_option_geopoint() {
    let origin = djogi::GeoPoint::new(47.6062, -122.3321).expect("valid");
    let _f = |p: PlaceFields| p
        .location()
        .explicit_pg_predicate()
        .within_km(origin, 25.0);
}

#[cfg(feature = "spatial")]
#[allow(dead_code)]
fn _order_by_distance_compiles_on_option_geopoint() {
    let origin = djogi::GeoPoint::new(47.6062, -122.3321).expect("valid");
    let _ord = |p: PlaceFields| p.location().order_by_distance(origin);
}

#[cfg(feature = "spatial")]
#[allow(dead_code)]
fn _is_null_still_works() {
    // Sanity check: existing IS NULL / IS NOT NULL surface remains
    // available on Option<GeoPoint> fields. The new spatial impls do
    // not shadow the generic `FieldRef<M, V>` is_null path.
    let _f1 = |p: PlaceFields| p.location().is_null();
    let _f2 = |p: PlaceFields| p.location().is_not_null();
}

fn main() {}
