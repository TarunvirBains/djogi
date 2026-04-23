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
//! - [`LineString`] — an ordered sequence of two or more points, stored as
//!   `GEOGRAPHY(LineString, 4326)`.
//! - [`GeographyValue`] — sealed trait implemented by all geometry types above.
//! - [`GeoError`] — errors from coordinate validation and EWKB codec failures.

mod ewkb;
pub mod linestring;
pub mod point;

pub use linestring::LineString;
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

    /// A `LineString` was constructed with fewer than the required number of
    /// points.
    ///
    /// `LineString` requires at least 2 distinct points. `got` is the number
    /// of points supplied; `need` is the minimum required.
    #[cfg(feature = "spatial")]
    #[error("invalid LineString: got {got} point(s), need at least {need}")]
    InvalidLineString {
        /// Number of points supplied by the caller.
        got: usize,
        /// Minimum number of points required (always 2).
        need: usize,
    },
}

// ── Sealed GeographyValue trait ───────────────────────────────────────────────

/// Private sealing module — its `Sealed` trait cannot be named outside this
/// crate, so `GeographyValue` cannot be implemented downstream.
#[cfg(feature = "spatial")]
mod sealed_value {
    pub trait Sealed {}
}

/// Sealed trait implemented by every Djogi geometry that maps to a
/// `GEOGRAPHY(..., 4326)` column.
///
/// ## Purpose
///
/// Query APIs that accept "any geography value" are generic over this trait
/// rather than enumerating concrete types. The seal prevents downstream crates
/// from inventing geometries that the query layer does not know how to emit or
/// decode. New geometry types ship via Djogi phases, not user code.
///
/// ## Wire format contract
///
/// Each implementor must round-trip through `GEOGRAPHY(<SUBTYPE>, 4326)` via
/// EWKB encoding. The `GEO_TYPE_WORD` constant embeds both the SRID flag
/// (`0x20000000`) and the base OGC geometry type number so the codec can
/// dispatch on a single `u32` comparison.
///
/// ## Geometry type words
///
/// | Type          | Base | With SRID flag   |
/// |---------------|------|------------------|
/// | Point         |    1 | `0x20000001`     |
/// | LineString    |    2 | `0x20000002`     |
/// | Polygon       |    3 | `0x20000003`     |
/// | MultiPoint    |    4 | `0x20000004`     |
/// | MultiPolygon  |    6 | `0x20000006`     |
#[cfg(feature = "spatial")]
pub trait GeographyValue: sealed_value::Sealed {
    /// EWKB type word including the SRID flag (`0x20000000` ORed with the
    /// base OGC geometry type number).
    const GEO_TYPE_WORD: u32;

    /// Descriptor-level subtype discriminant from [`crate::descriptor::GeographySubtype`].
    const SUBTYPE: crate::descriptor::GeographySubtype;

    /// Encode `self` into its EWKB wire format (little-endian, SRID 4326).
    fn to_ewkb_bytes(&self) -> Vec<u8>;

    /// Decode an EWKB buffer into `Self`.
    ///
    /// Returns an error if the type word does not match `GEO_TYPE_WORD`,
    /// the SRID is not 4326, or the coordinate data is structurally invalid.
    fn from_ewkb_bytes(bytes: &[u8]) -> Result<Self, GeoError>
    where
        Self: Sized;
}

// ── GeoPoint impl ─────────────────────────────────────────────────────────────

#[cfg(feature = "spatial")]
impl sealed_value::Sealed for GeoPoint {}

#[cfg(feature = "spatial")]
impl GeographyValue for GeoPoint {
    const GEO_TYPE_WORD: u32 = 0x20000001;
    const SUBTYPE: crate::descriptor::GeographySubtype = crate::descriptor::GeographySubtype::Point;

    fn to_ewkb_bytes(&self) -> Vec<u8> {
        GeoPoint::to_ewkb_bytes(*self)
    }

    fn from_ewkb_bytes(bytes: &[u8]) -> Result<Self, GeoError> {
        GeoPoint::from_ewkb_bytes(bytes)
    }
}

// ── LineString impl ───────────────────────────────────────────────────────────

#[cfg(feature = "spatial")]
impl sealed_value::Sealed for LineString {}

#[cfg(feature = "spatial")]
impl GeographyValue for LineString {
    const GEO_TYPE_WORD: u32 = 0x20000002;
    const SUBTYPE: crate::descriptor::GeographySubtype =
        crate::descriptor::GeographySubtype::LineString;

    fn to_ewkb_bytes(&self) -> Vec<u8> {
        ewkb::encode_linestring(self)
    }

    fn from_ewkb_bytes(bytes: &[u8]) -> Result<Self, GeoError> {
        ewkb::decode_linestring(bytes)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "spatial"))]
mod geography_value_tests {
    use super::*;

    fn takes_geo<G: GeographyValue>() {}

    #[test]
    fn geopoint_is_geography_value() {
        takes_geo::<GeoPoint>();
    }

    #[test]
    fn linestring_is_geography_value() {
        takes_geo::<LineString>();
    }
}
