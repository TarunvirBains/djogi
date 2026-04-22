//! Spatial types for Djogi — gated behind the `spatial` feature flag.
//!
//! Enable with:
//!
//! ```toml
//! djogi = { version = "…", features = ["spatial"] }
//! ```
//!
//! # What is in this module
//!
//! - [`GeoPoint`] — a WGS-84 latitude/longitude coordinate, stored as
//!   `GEOGRAPHY(Point, 4326)` in Postgres.
//! - [`GeoError`] — errors from coordinate validation and EWKB codec failures.
//!
//! # Future work
//!
//! The `spatial` feature flag is intentionally narrow at first: only `GeoPoint`
//! ships in Phase 6. Polygon, linestring, and multipoint support — along with
//! index-backed spatial query operators — will arrive in a later phase.

mod ewkb;
pub mod point;

pub use point::GeoPoint;

use thiserror::Error;

/// Errors produced by the `geo` module.
///
/// All variants are `#[non_exhaustive]` at the enum level so callers must use
/// a wildcard arm when matching — this preserves forward compatibility as new
/// spatial types (and new error conditions) are added.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GeoError {
    /// The latitude value is out of range or non-finite.
    ///
    /// Valid latitude is any finite `f64` in `-90.0..=90.0`.
    #[error("invalid latitude {0}: must be finite and in -90.0..=90.0")]
    InvalidLatitude(f64),

    /// The longitude value is out of range or non-finite.
    ///
    /// Valid longitude is any finite `f64` in `-180.0..=180.0`.
    #[error("invalid longitude {0}: must be finite and in -180.0..=180.0")]
    InvalidLongitude(f64),

    /// The EWKB buffer was structurally invalid.
    ///
    /// The message describes the specific byte-level mismatch: wrong total
    /// length, unexpected endianness byte, or unrecognised geometry type word.
    #[error("malformed EWKB: {0}")]
    MalformedEwkb(String),

    /// The EWKB buffer decoded correctly but carried an SRID other than 4326.
    ///
    /// Djogi only accepts WGS-84 geography (`SRID = 4326`). If a column was
    /// inserted with a different SRID, this error surfaces the actual SRID so
    /// the caller can diagnose the mismatch.
    #[error("unexpected SRID {0}: Djogi requires SRID 4326 (WGS-84)")]
    UnexpectedSrid(u32),
}
