//! Compile-fail: `djogi::geo` and `djogi::GeoPoint` are not accessible without
//! the `spatial` feature flag.
//!
//! Without `djogi = { features = ["spatial"] }`, importing `djogi::geo::GeoPoint`
//! must fail with a clean "unresolved import" diagnostic. The lihaaf runner
//! compiles this fixture using the default-feature djogi dev-dep (no `spatial`),
//! so the import error is expected and the `.stderr` file locks the exact
//! diagnostic shape.

use djogi::geo::GeoPoint;

fn main() {
 let _ = GeoPoint::new(0.0, 0.0);
}
