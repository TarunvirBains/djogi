//! Spatial types for Djogi — gated behind the `spatial` feature flag.
//! Enable with:
//! ```toml
//! djogi = { version = "…", features = ["spatial"] }
//! ```
//! # What is in this module
//! - [`GeoPoint`] — a WGS-84 latitude/longitude coordinate, stored as
//! `GEOGRAPHY(Point, 4326)` in Postgres.
//! - [`LineString`] — an ordered sequence of two or more points, stored as
//! `GEOGRAPHY(LineString, 4326)`.
//! - [`Polygon`] — a closed ring (with optional holes), stored as
//! `GEOGRAPHY(Polygon, 4326)`.
//! - [`MultiPoint`] — an unordered collection of one or more points, stored as
//! `GEOGRAPHY(MultiPoint, 4326)`.
//! - [`MultiPolygon`] — a collection of one or more polygons, stored as
//! `GEOGRAPHY(MultiPolygon, 4326)`.
//! - [`GeographyValue`] — sealed trait implemented by all geometry types above.
//! - [`GeoError`] — errors from coordinate validation and EWKB codec failures.

mod ewkb;
pub mod linestring;
pub mod multilinestring;
pub mod multipoint;
pub mod multipolygon;
pub mod point;
pub mod polygon;

pub use linestring::LineString;
pub use multilinestring::MultiLineString;
pub use multipoint::MultiPoint;
pub use multipolygon::MultiPolygon;
pub use point::GeoPoint;
pub use polygon::Polygon;

use thiserror::Error;

/// Errors produced by the `geo` module.
/// All variants are `#[non_exhaustive]` at the enum level so callers must use
/// a wildcard arm when matching — this preserves forward compatibility as new
/// spatial types (and new error conditions) are added.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GeoError {
    /// The latitude value is out of range or non-finite.
    /// Valid latitude is any finite `f64` in `-90.0..=90.0`.
    #[error("invalid latitude {0}: must be finite and in -90.0..=90.0")]
    InvalidLatitude(f64),

    /// The longitude value is out of range or non-finite.
    /// Valid longitude is any finite `f64` in `-180.0..=180.0`.
    #[error("invalid longitude {0}: must be finite and in -180.0..=180.0")]
    InvalidLongitude(f64),

    /// The EWKB buffer was structurally invalid.
    /// The message describes the specific byte-level mismatch: wrong total
    /// length, unexpected endianness byte, or unrecognised geometry type word.
    #[error("malformed EWKB: {0}")]
    MalformedEwkb(String),

    /// The EWKB buffer decoded correctly but carried an SRID other than 4326.
    /// Djogi only accepts WGS-84 geography (`SRID = 4326`). If a column was
    /// inserted with a different SRID, this error surfaces the actual SRID so
    /// the caller can diagnose the mismatch.
    #[error("unexpected SRID {0}: Djogi requires SRID 4326 (WGS-84)")]
    UnexpectedSrid(u32),

    /// A `LineString` was constructed with fewer than the required number of
    /// points.
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

    /// A `Polygon` ring failed validation.
    /// The `reason` string describes the specific constraint that was violated:
    /// insufficient vertices, an unclosed ring, or a hole that fails the same
    /// constraints as the outer ring.
    #[cfg(feature = "spatial")]
    #[error("invalid Polygon: {reason}")]
    InvalidPolygon {
        /// Human-readable description of the violated constraint.
        reason: &'static str,
    },

    /// A `MultiPoint` was constructed with an empty point list.
    /// `MultiPoint` requires at least one point.
    #[cfg(feature = "spatial")]
    #[error("invalid MultiPoint: {reason}")]
    InvalidMultiPoint {
        /// Human-readable description of the violated constraint.
        reason: &'static str,
    },

    /// A `MultiPolygon` was constructed with an empty polygon list.
    /// `MultiPolygon` requires at least one polygon.
    #[cfg(feature = "spatial")]
    #[error("invalid MultiPolygon: {reason}")]
    InvalidMultiPolygon {
        /// Human-readable description of the violated constraint.
        reason: &'static str,
    },

    /// A `MultiLineString` was constructed with an empty linestring list.
    /// `MultiLineString` requires at least one `LineString`. Mirrors the
    /// non-empty invariant on `MultiPoint` and `MultiPolygon`.
    #[cfg(feature = "spatial")]
    #[error("invalid MultiLineString: {reason}")]
    InvalidMultiLineString {
        /// Human-readable description of the violated constraint.
        reason: &'static str,
    },
}

// ── postgres_types acceptance helper ──────────────────────────────────────────

/// The Postgres type name every `GEOGRAPHY(*, 4326)` column reports.
#[cfg(feature = "spatial")]
pub(crate) const GEOGRAPHY_TYPE_NAME: &str = "geography";

/// Shared `ToSql::accepts` / `FromSql::accepts` predicate for every geometry
/// type. Centralising this keeps the `"geography"` literal in one place so a
/// future PostGIS naming change touches a single line, not ten.
#[cfg(feature = "spatial")]
pub(crate) fn accepts_geography(ty: &postgres_types::Type) -> bool {
    ty.name() == GEOGRAPHY_TYPE_NAME
}

// ── ToSql / FromSql codec macro ──────────────────────────────────────────────

/// Emit `ToSql` and `FromSql` impls for a geometry type that round-trips
/// through `GEOGRAPHY(*, 4326)`.
/// Every Djogi geometry uses the identical codec pattern: write EWKB into the
/// outbound `BytesMut` for `to_sql`, read EWKB via `from_ewkb_bytes` for
/// `from_sql`, accept any column whose Postgres type name is `"geography"`.
/// Spelling each one out by hand was ~30 lines per type × 5 types = 150
/// duplicated lines; this macro collapses them into single-line invocations.
/// `$encode_into` is the path to a `pub(crate) fn(&Self, &mut impl BufMut)`
/// that writes the EWKB representation directly into the bind buffer — no
/// intermediate `Vec<u8>` allocation.
#[cfg(feature = "spatial")]
macro_rules! impl_geography_codec {
    ($t:ty, $encode_into:path) => {
        impl ::postgres_types::ToSql for $t {
            fn to_sql(
                &self,
                _ty: &::postgres_types::Type,
                out: &mut ::bytes::BytesMut,
            ) -> ::std::result::Result<
                ::postgres_types::IsNull,
                ::std::boxed::Box<
                    dyn ::std::error::Error + ::std::marker::Sync + ::std::marker::Send,
                >,
            > {
                $encode_into(self, out);
                Ok(::postgres_types::IsNull::No)
            }

            fn accepts(ty: &::postgres_types::Type) -> bool {
                $crate::geo::accepts_geography(ty)
            }

            ::postgres_types::to_sql_checked!();
        }

        impl<'a> ::postgres_types::FromSql<'a> for $t {
            fn from_sql(
                _ty: &::postgres_types::Type,
                raw: &'a [u8],
            ) -> ::std::result::Result<
                Self,
                ::std::boxed::Box<
                    dyn ::std::error::Error + ::std::marker::Sync + ::std::marker::Send,
                >,
            > {
                <$t>::from_ewkb_bytes(raw).map_err(|e| ::std::boxed::Box::new(e) as _)
            }

            fn accepts(ty: &::postgres_types::Type) -> bool {
                $crate::geo::accepts_geography(ty)
            }
        }
    };
}

#[cfg(feature = "spatial")]
pub(crate) use impl_geography_codec;

// ── Sealed GeographyValue trait ───────────────────────────────────────────────

/// Private sealing module — its `Sealed` trait cannot be named outside this
/// crate, so `GeographyValue` cannot be implemented downstream.
#[cfg(feature = "spatial")]
mod sealed_value {
    pub trait Sealed {}
}

/// Sealed trait implemented by every Djogi geometry that maps to a
/// `GEOGRAPHY(..., 4326)` column.
/// ## Purpose
/// Query APIs that accept "any geography value" are generic over this trait
/// rather than enumerating concrete types. The seal prevents downstream crates
/// from inventing geometries that the query layer does not know how to emit or
/// decode. New geometry types ship via Djogi phases, not user code.
/// ## Wire format contract
/// Each implementor must round-trip through `GEOGRAPHY(<SUBTYPE>, 4326)` via
/// EWKB encoding. The `GEO_TYPE_WORD` constant embeds both the SRID flag
/// (`0x20000000`) and the base OGC geometry type number so the codec can
/// dispatch on a single `u32` comparison.
/// ## Geometry type words
/// | Type | Base | With SRID flag |
/// |---------------|------|------------------|
/// | Point | 1 | `0x20000001` |
/// | LineString | 2 | `0x20000002` |
/// | Polygon | 3 | `0x20000003` |
/// | MultiPoint | 4 | `0x20000004` |
/// | MultiPolygon | 6 | `0x20000006` |
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
        LineString::to_ewkb_bytes(self)
    }

    fn from_ewkb_bytes(bytes: &[u8]) -> Result<Self, GeoError> {
        ewkb::decode_linestring(bytes)
    }
}

// ── Polygon impl ──────────────────────────────────────────────────────────────

#[cfg(feature = "spatial")]
impl sealed_value::Sealed for Polygon {}

#[cfg(feature = "spatial")]
impl GeographyValue for Polygon {
    const GEO_TYPE_WORD: u32 = 0x20000003;
    const SUBTYPE: crate::descriptor::GeographySubtype =
        crate::descriptor::GeographySubtype::Polygon;

    fn to_ewkb_bytes(&self) -> Vec<u8> {
        Polygon::to_ewkb_bytes(self)
    }

    fn from_ewkb_bytes(bytes: &[u8]) -> Result<Self, GeoError> {
        ewkb::decode_polygon(bytes)
    }
}

// ── MultiPoint impl ───────────────────────────────────────────────────────────

#[cfg(feature = "spatial")]
impl sealed_value::Sealed for MultiPoint {}

#[cfg(feature = "spatial")]
impl GeographyValue for MultiPoint {
    const GEO_TYPE_WORD: u32 = 0x20000004;
    const SUBTYPE: crate::descriptor::GeographySubtype =
        crate::descriptor::GeographySubtype::MultiPoint;

    fn to_ewkb_bytes(&self) -> Vec<u8> {
        MultiPoint::to_ewkb_bytes(self)
    }

    fn from_ewkb_bytes(bytes: &[u8]) -> Result<Self, GeoError> {
        ewkb::decode_multipoint(bytes)
    }
}

// ── MultiLineString impl ──────────────────────────────────────────────────────

#[cfg(feature = "spatial")]
impl sealed_value::Sealed for MultiLineString {}

#[cfg(feature = "spatial")]
impl GeographyValue for MultiLineString {
    const GEO_TYPE_WORD: u32 = 0x20000005;
    const SUBTYPE: crate::descriptor::GeographySubtype =
        crate::descriptor::GeographySubtype::MultiLineString;

    fn to_ewkb_bytes(&self) -> Vec<u8> {
        MultiLineString::to_ewkb_bytes(self)
    }

    fn from_ewkb_bytes(bytes: &[u8]) -> Result<Self, GeoError> {
        ewkb::decode_multilinestring(bytes)
    }
}

// ── MultiPolygon impl ─────────────────────────────────────────────────────────

#[cfg(feature = "spatial")]
impl sealed_value::Sealed for MultiPolygon {}

#[cfg(feature = "spatial")]
impl GeographyValue for MultiPolygon {
    const GEO_TYPE_WORD: u32 = 0x20000006;
    const SUBTYPE: crate::descriptor::GeographySubtype =
        crate::descriptor::GeographySubtype::MultiPolygon;

    fn to_ewkb_bytes(&self) -> Vec<u8> {
        MultiPolygon::to_ewkb_bytes(self)
    }

    fn from_ewkb_bytes(bytes: &[u8]) -> Result<Self, GeoError> {
        ewkb::decode_multipolygon(bytes)
    }
}

// ── Sealed SpatialColumnValue trait — admits Option<G> nullable spatial columns ─

/// Private sealing module for [`SpatialColumnValue`]. Downstream crates
/// cannot name `Sealed`, so `SpatialColumnValue` stays a closed set.
#[cfg(feature = "spatial")]
mod sealed_spatial_column_value {
    pub trait Sealed {}
}

/// Sealed marker trait: this value type is valid as the Rust-side type
/// of a SQL geography column referenced by a pair-tuple spatial
/// annotation (e.g. [`crate::query::PairAreaOverlapRatio`]).
/// # Why a separate trait from [`GeographyValue`]
/// `GeographyValue` is implemented by concrete geometry types
/// (`Polygon`, `GeoPoint`, …) and carries an EWKB encode/decode
/// contract used by the scalar `Expr::area_of` / `area_of_intersection`
/// constructors that bind already-known geometries as `bytea` literals.
/// `SpatialColumnValue` is for the *column-reference* case: the
/// annotation only needs the column's static name; the actual value
/// lives in the row and is materialised by Postgres. A column can be
/// non-nullable (`territory: Polygon`) or nullable (`territory:
/// Option<Polygon>`); both are valid column types, but `Option<G>`
/// doesn't satisfy `GeographyValue` (encoding a `None` to EWKB has no
/// meaning at the type level — it would be a NULL value, which the
/// scalar path handles via `bytea` NULL binds, not Rust-side encoding).
/// Splitting the bound keeps the scalar API's tight EWKB contract
/// intact (every `GeographyValue` round-trips a concrete geometry)
/// while letting the column-reference API accept nullable columns
/// the common case in adopter schemas where territory polygons may
/// not yet be materialised for every row.
/// # Implementations
/// - Every `G: GeographyValue` (the bare-column case, e.g.
/// `territory: Polygon`).
/// - `Option<G>` for every `G: GeographyValue` (the nullable-column
/// case, e.g. `territory: Option<Polygon>`).
/// # Sealing rationale
/// `Sealed` lives in [`sealed_spatial_column_value`], a `pub(crate)`
/// module — downstream crates can name the trait as a bound but cannot
/// add new impls. This matches the seal on `GeographyValue` and keeps
/// the typed pair-spatial annotation surface from being extended via
/// user code injecting a non-spatial value type.
#[cfg(feature = "spatial")]
pub trait SpatialColumnValue: sealed_spatial_column_value::Sealed {}

#[cfg(feature = "spatial")]
impl<G> sealed_spatial_column_value::Sealed for G where G: GeographyValue {}

#[cfg(feature = "spatial")]
impl<G> SpatialColumnValue for G where G: GeographyValue {}

#[cfg(feature = "spatial")]
impl<G> sealed_spatial_column_value::Sealed for Option<G> where G: GeographyValue {}

#[cfg(feature = "spatial")]
impl<G> SpatialColumnValue for Option<G> where G: GeographyValue {}

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

    #[test]
    fn polygon_is_geography_value() {
        takes_geo::<Polygon>();
    }

    #[test]
    fn multipoint_is_geography_value() {
        takes_geo::<MultiPoint>();
    }

    #[test]
    fn multilinestring_is_geography_value() {
        takes_geo::<MultiLineString>();
    }

    #[test]
    fn multipolygon_is_geography_value() {
        takes_geo::<MultiPolygon>();
    }
}
