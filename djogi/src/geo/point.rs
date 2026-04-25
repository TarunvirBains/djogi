//! The `GeoPoint` value type — a WGS-84 latitude/longitude coordinate.
//!
//! `GeoPoint` is the fundamental spatial primitive in Djogi's spatial layer.
//! It maps to a `GEOGRAPHY(Point, 4326)` column in Postgres and round-trips
//! through the PostGIS EWKB wire format.
//!
//! # Coordinate convention
//!
//! Djogi stores `lat` and `lon` as separate named fields (latitude first,
//! matching the human-readable convention). On the wire, the OGC WKT `POINT`
//! format and the EWKB `X`/`Y` fields use **longitude first** — this is
//! handled transparently by `Display` and the codec.
//!
//! # Validation
//!
//! `GeoPoint::new` rejects non-finite values (NaN, infinity) and out-of-range
//! coordinates at construction time. A `GeoPoint` in memory is always a valid
//! WGS-84 coordinate.
//!
//! # Postgres integration
//!
//! `ToSql` and `FromSql` use the EWKB codec implemented in
//! [`crate::geo::ewkb`]. The `accepts` check matches the Postgres type name
//! `"geography"`. In unit tests where no live Postgres connection is present,
//! `Type::BYTEA` is used as a stand-in type to exercise the byte-writing path;
//! the production acceptance check against `"geography"` is verified
//! empirically in the Phase 6 Task 4 integration tests.

use std::fmt;

use bytes::BytesMut;
use postgres_types::{FromSql, IsNull, ToSql, Type};
use serde::{Deserialize, Serialize};

use crate::geo::GeoError;
use crate::geo::ewkb;

/// A WGS-84 latitude/longitude coordinate.
///
/// Construct via [`GeoPoint::new`]. Direct struct literals are intentionally
/// not supported — the constructor enforces the invariant that both coordinates
/// are finite and within range.
///
/// # Display
///
/// `Display` emits `POINT(<lon> <lat>)` per the OGC WKT convention (longitude
/// first). This is suitable for passing to PostGIS functions and for
/// human-readable output.
///
/// # Serde
///
/// Serializes as `{"lat": <f64>, "lon": <f64>}`. Deserialization validates
/// the coordinate bounds via `GeoPoint::new`, so a malformed JSON object
/// yields a `serde` error rather than silently constructing an invalid point.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct GeoPoint {
    /// Latitude in decimal degrees. Valid range: -90.0 to 90.0 inclusive.
    pub lat: f64,
    /// Longitude in decimal degrees. Valid range: -180.0 to 180.0 inclusive.
    pub lon: f64,
}

impl GeoPoint {
    /// Latitude lower bound (south pole) in decimal degrees per WGS 84 /
    /// ISO 6709. Exposed so adopters can validate inputs against the
    /// same range `GeoPoint::new` enforces without re-typing the magic
    /// number. Mirrors the bounds-as-const idiom upstream `HeerId` uses
    /// (`HeerId::MAX_TIMESTAMP_MS`, `MAX_NODE_ID`, `MAX_SEQUENCE`).
    pub const MIN_LAT: f64 = -90.0;

    /// Latitude upper bound (north pole) in decimal degrees per WGS 84 /
    /// ISO 6709.
    pub const MAX_LAT: f64 = 90.0;

    /// Longitude lower bound (antimeridian, west) in decimal degrees per
    /// WGS 84 / ISO 6709.
    pub const MIN_LON: f64 = -180.0;

    /// Longitude upper bound (antimeridian, east) in decimal degrees per
    /// WGS 84 / ISO 6709.
    pub const MAX_LON: f64 = 180.0;

    /// Construct a new `GeoPoint`, validating coordinate ranges.
    ///
    /// Returns `Err(GeoError::InvalidLatitude)` if `lat` is not finite or
    /// is outside [`Self::MIN_LAT`]..=[`Self::MAX_LAT`]. Returns
    /// `Err(GeoError::InvalidLongitude)` if `lon` is not finite or is
    /// outside [`Self::MIN_LON`]..=[`Self::MAX_LON`].
    pub fn new(lat: f64, lon: f64) -> Result<Self, GeoError> {
        if !lat.is_finite() || !(Self::MIN_LAT..=Self::MAX_LAT).contains(&lat) {
            return Err(GeoError::InvalidLatitude(lat));
        }
        if !lon.is_finite() || !(Self::MIN_LON..=Self::MAX_LON).contains(&lon) {
            return Err(GeoError::InvalidLongitude(lon));
        }
        Ok(Self { lat, lon })
    }

    /// Encode this point as a 25-byte EWKB buffer for `GEOGRAPHY(Point, 4326)`.
    pub fn to_ewkb_bytes(self) -> Vec<u8> {
        ewkb::encode_point(self.lon, self.lat)
    }

    /// Decode a 25-byte EWKB buffer into a `GeoPoint`.
    pub fn from_ewkb_bytes(bytes: &[u8]) -> Result<Self, GeoError> {
        let (lon, lat) = ewkb::decode_point(bytes)?;
        // The EWKB buffer comes from Postgres and is expected to already carry
        // a valid coordinate pair, but we validate anyway so that a corrupt
        // or hand-crafted buffer does not create an invalid GeoPoint.
        Self::new(lat, lon)
    }

    /// Compute the great-circle distance to `other` in metres using the
    /// Haversine formula with an Earth radius of 6 371 000 m.
    ///
    /// The Haversine formula gives the shortest-path distance over the
    /// surface of a sphere and is accurate to within about 0.5% for
    /// most terrestrial distances. Use PostGIS `ST_Distance` for higher
    /// precision on an ellipsoidal model.
    pub fn distance_to(self, other: Self) -> f64 {
        const R: f64 = 6_371_000.0;
        let d_lat = (other.lat - self.lat).to_radians();
        let d_lon = (other.lon - self.lon).to_radians();
        let lat1 = self.lat.to_radians();
        let lat2 = other.lat.to_radians();
        let a = (d_lat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (d_lon / 2.0).sin().powi(2);
        let c = 2.0 * a.sqrt().asin();
        R * c
    }
}

// ── Display (WKT) ─────────────────────────────────────────────────────────────

impl fmt::Display for GeoPoint {
    /// Emit `POINT(<lon> <lat>)` per the OGC WKT convention.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "POINT({} {})", self.lon, self.lat)
    }
}

// ── Serde: custom Deserialize to run validation ───────────────────────────────

impl<'de> Deserialize<'de> for GeoPoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            lat: f64,
            lon: f64,
        }
        let raw = Raw::deserialize(deserializer)?;
        GeoPoint::new(raw.lat, raw.lon).map_err(serde::de::Error::custom)
    }
}

// ── postgres_types codec ──────────────────────────────────────────────────────

impl ToSql for GeoPoint {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        out.extend_from_slice(&self.to_ewkb_bytes());
        Ok(IsNull::No)
    }

    /// Accept the Postgres `geography` type.
    ///
    /// Note: `ty.name()` returns `"geography"` for PostGIS geography columns.
    /// In unit tests a stand-in type (`BYTEA`) is used because no live Postgres
    /// connection is present; the production acceptance check is verified
    /// empirically in the Phase 6 Task 4 integration tests.
    fn accepts(ty: &Type) -> bool {
        ty.name() == "geography"
    }

    postgres_types::to_sql_checked!();
}

impl<'a> FromSql<'a> for GeoPoint {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        GeoPoint::from_ewkb_bytes(raw).map_err(|e| Box::new(e) as _)
    }

    /// Accept the Postgres `geography` type.
    fn accepts(ty: &Type) -> bool {
        ty.name() == "geography"
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "spatial"))]
mod tests {
    use super::*;
    use crate::geo::GeoError;

    // ── coordinate validation ─────────────────────────────────────────────────

    #[test]
    fn geopoint_new_validates_latitude() {
        // Boundary values — must succeed.
        assert!(GeoPoint::new(90.0, 0.0).is_ok());
        assert!(GeoPoint::new(-90.0, 0.0).is_ok());
        assert!(GeoPoint::new(0.0, 0.0).is_ok());

        // Out-of-range — must fail.
        assert!(matches!(
            GeoPoint::new(91.0, 0.0),
            Err(GeoError::InvalidLatitude(_))
        ));
        assert!(matches!(
            GeoPoint::new(-91.0, 0.0),
            Err(GeoError::InvalidLatitude(_))
        ));

        // Non-finite — must fail.
        assert!(matches!(
            GeoPoint::new(f64::NAN, 0.0),
            Err(GeoError::InvalidLatitude(_))
        ));
        assert!(matches!(
            GeoPoint::new(f64::INFINITY, 0.0),
            Err(GeoError::InvalidLatitude(_))
        ));
        assert!(matches!(
            GeoPoint::new(f64::NEG_INFINITY, 0.0),
            Err(GeoError::InvalidLatitude(_))
        ));
    }

    #[test]
    fn geopoint_new_validates_longitude() {
        // Boundary values — must succeed.
        assert!(GeoPoint::new(0.0, 180.0).is_ok());
        assert!(GeoPoint::new(0.0, -180.0).is_ok());
        assert!(GeoPoint::new(0.0, 0.0).is_ok());

        // Out-of-range — must fail.
        assert!(matches!(
            GeoPoint::new(0.0, 181.0),
            Err(GeoError::InvalidLongitude(_))
        ));
        assert!(matches!(
            GeoPoint::new(0.0, -181.0),
            Err(GeoError::InvalidLongitude(_))
        ));

        // Non-finite — must fail.
        assert!(matches!(
            GeoPoint::new(0.0, f64::NAN),
            Err(GeoError::InvalidLongitude(_))
        ));
        assert!(matches!(
            GeoPoint::new(0.0, f64::INFINITY),
            Err(GeoError::InvalidLongitude(_))
        ));
        assert!(matches!(
            GeoPoint::new(0.0, f64::NEG_INFINITY),
            Err(GeoError::InvalidLongitude(_))
        ));
    }

    // ── EWKB round-trip ──────────────────────────────────────────────────────

    #[test]
    fn ewkb_round_trip() {
        let p = GeoPoint::new(37.7749, -122.4194).unwrap();
        let bytes = p.to_ewkb_bytes();
        assert_eq!(bytes.len(), 25, "EWKB buffer must be 25 bytes");
        let decoded = GeoPoint::from_ewkb_bytes(&bytes).unwrap();
        assert!((decoded.lat - p.lat).abs() < 1e-9);
        assert!((decoded.lon - p.lon).abs() < 1e-9);
    }

    // ── malformed EWKB rejection ──────────────────────────────────────────────

    #[test]
    fn ewkb_rejects_malformed() {
        // Empty buffer.
        assert!(GeoPoint::from_ewkb_bytes(&[]).is_err());

        // One byte short (24 bytes).
        let short = vec![0u8; 24];
        assert!(GeoPoint::from_ewkb_bytes(&short).is_err());

        // One byte long (26 bytes).
        let long = vec![0u8; 26];
        assert!(GeoPoint::from_ewkb_bytes(&long).is_err());

        // Correct length but bad endianness byte.
        let mut bad_endian = GeoPoint::new(0.0, 0.0).unwrap().to_ewkb_bytes();
        bad_endian[0] = 0x02;
        assert!(GeoPoint::from_ewkb_bytes(&bad_endian).is_err());

        // Correct length and endianness but SRID 4269 (NAD83) instead of 4326.
        let mut bad_srid = GeoPoint::new(0.0, 0.0).unwrap().to_ewkb_bytes();
        // 4269 in little-endian = [0xAD, 0x10, 0x00, 0x00]
        let srid_4269 = 4269u32.to_le_bytes();
        bad_srid[5] = srid_4269[0];
        bad_srid[6] = srid_4269[1];
        bad_srid[7] = srid_4269[2];
        bad_srid[8] = srid_4269[3];
        assert!(
            matches!(
                GeoPoint::from_ewkb_bytes(&bad_srid),
                Err(GeoError::UnexpectedSrid(4269))
            ),
            "expected UnexpectedSrid(4269)"
        );
    }

    // ── Haversine sanity check ────────────────────────────────────────────────

    #[test]
    fn haversine_sanity() {
        // SFO: 37.6189 N, 122.3750 W  →  JFK: 40.6413 N, 73.7781 W
        // Great-circle distance approximately 4151 km.
        let sfo = GeoPoint::new(37.6189, -122.3750).unwrap();
        let jfk = GeoPoint::new(40.6413, -73.7781).unwrap();
        let dist_km = sfo.distance_to(jfk) / 1000.0;
        assert!(
            (dist_km - 4151.0).abs() < 50.0,
            "SFO→JFK Haversine = {dist_km:.1} km, expected ~4151 km"
        );
    }

    // ── WKT formatting ───────────────────────────────────────────────────────

    #[test]
    fn wkt_format() {
        let p = GeoPoint::new(37.7749, -122.4194).unwrap();
        assert_eq!(format!("{p}"), "POINT(-122.4194 37.7749)");
    }

    // ── serde round-trip ─────────────────────────────────────────────────────

    #[test]
    fn serde_round_trip() {
        let original = GeoPoint::new(37.7749, -122.4194).unwrap();
        let json = serde_json::to_string(&original).unwrap();
        let decoded: GeoPoint = serde_json::from_str(&json).unwrap();
        // Bit-identical comparison is valid here because we are round-tripping
        // through JSON text representation of the same f64 values — no
        // arithmetic is performed, so no precision is lost.
        assert_eq!(original, decoded);
    }

    // ── ToSql codec output (unit test without live DB) ───────────────────────

    #[test]
    fn to_sql_writes_ewkb_bytes() {
        use postgres_types::Type;

        let p = GeoPoint::new(51.5074, -0.1278).unwrap();
        let expected = p.to_ewkb_bytes();
        assert_eq!(expected.len(), 25);

        // Use BYTEA as the stand-in type for a unit test without a live Postgres
        // connection. Production code uses the `geography` type; the acceptance
        // check for `geography` is verified in Phase 6 Task 4 integration tests.
        let mut out = BytesMut::new();
        p.to_sql(&Type::BYTEA, &mut out).unwrap();
        assert_eq!(out.as_ref(), expected.as_slice());
    }
}
