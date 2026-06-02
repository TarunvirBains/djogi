//! Compile-fail smoke test: `djogi::geo::GeoPoint` is not reachable without
//! the `spatial` feature flag.
//!
//! A default-features consumer that tries to import `djogi::geo::GeoPoint`
//! must get a clean "unresolved import `djogi::geo`" diagnostic, confirming
//! that the spatial surface does not leak into builds that did not opt in.
//!
//! The lihaaf runner compiles this fixture against the default-feature
//! `djogi` dev-dep (no `spatial`), so the import error is expected and the
//! `.stderr` file locks the exact diagnostic shape.

use djogi::geo::GeoPoint;

fn main() {
    let _ = GeoPoint::new(0.0, 0.0);
}
