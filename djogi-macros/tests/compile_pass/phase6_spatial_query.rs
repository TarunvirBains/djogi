//! Compile-pass: PostGIS-specific `within_km` is reachable through the
//! explicit SQL-only predicate route, while `order_by_distance` remains
//! available on the root field handle.
//!
//! Both methods are gated on `#[cfg(feature = "spatial")]`. When the feature
//! is off, the `Place` struct definition is elided and only `fn main() {}` is
//! compiled — the file itself still compiles cleanly.
//!
//! Runtime invariants (SQL shape, bind-parameter count, deterministic
//! ordering) are verified in `djogi/src/expr/spatial.rs#[cfg(test)]` and
//! `tests/integration/phase6_spatial.rs`.

#[cfg(feature = "spatial")]
use djogi::prelude::*;

#[cfg(feature = "spatial")]
#[allow(dead_code)]
// `no_default` is required because GeoPoint does not implement Default.
#[model(table = "places", no_default)]
#[derive(Debug, Clone)]
pub struct Place {
    pub name: String,
    pub location: djogi::GeoPoint,
}

fn main() {
    #[cfg(feature = "spatial")]
    {
        let center = djogi::GeoPoint::new(37.7749, -122.4194).unwrap();

        // `within_km` is PostGIS-specific, so it lives behind
        // `explicit_pg_predicate()` rather than the portable root-field
        // predicate surface. Compile-time check only — no DB call.
        let _qs_filter =
            Place::objects().filter(|p| p.location().explicit_pg_predicate().within_km(center, 5.0));

        // `order_by_distance` must be accepted by `order_by` closure.
        let _qs_order = Place::objects().order_by(|p| p.location().order_by_distance(center));

        // Both compose on the same QuerySet.
        let _qs_both = Place::objects()
            .filter(|p| p.location().explicit_pg_predicate().within_km(center, 50.0))
            .order_by(|p| p.location().order_by_distance(center));
    }
}
