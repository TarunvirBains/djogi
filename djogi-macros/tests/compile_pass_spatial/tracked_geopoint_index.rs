//! Compile-pass: the `#[model]` macro accepts a dirty-tracked `GeoPoint`
//! field (`Tracked<GeoPoint>`) annotated `#[field(index)]` and emits valid
//! Rust without any compile errors.
//!
//! Regression guard for djogi#468: the geography index classifiers strip the
//! transparent `Tracked<_>` wrapper via `unwrap_schema_type`, so a dirty-tracked
//! geometry column resolves to a GiST index — not the BTree index a
//! non-stripping classifier would pick.
//!
//! The model definition is wrapped in `#[cfg(feature = "spatial")]` so this
//! fixture compiles under both `--features spatial` (full expansion exercised)
//! and default features (the struct is gated out, but the file itself still
//! compiles cleanly with just `fn main() {}`).
//!
//! `no_default` is required because `GeoPoint` does not implement `Default`.
//! `Tracked` is re-exported through `djogi::prelude`.

#[cfg(feature = "spatial")]
use djogi::prelude::*;

#[cfg(feature = "spatial")]
#[allow(dead_code)]
#[model(table = "tracked_geopoint_index_places", no_default)]
#[derive(Debug, Clone)]
pub struct Place {
    pub name: String,
    #[field(index)]
    pub location: Tracked<djogi::GeoPoint>,
}

fn main() {}
