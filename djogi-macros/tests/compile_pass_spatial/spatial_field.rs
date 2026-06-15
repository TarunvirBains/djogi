//! Compile-pass: the `#[model]` macro accepts a `GeoPoint` field type and
//! emits valid Rust without any compile errors.
//!
//! The model definition is wrapped in `#[cfg(feature = "spatial")]` so this
//! fixture compiles under both `--features spatial` (full expansion exercised)
//! and default features (the struct is gated out, but the file itself still
//! compiles cleanly with just `fn main() {}`).
//!
//! The runtime invariant (descriptor `sql_type == Geography { srid: 4326 }`
//! and the GiST index entry) is verified in
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

fn main() {}
