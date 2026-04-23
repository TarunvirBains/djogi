//! `LineString` — an ordered sequence of two or more `GeoPoint` values.
//!
//! Stored as `GEOGRAPHY(LineString, 4326)` in Postgres. The type enforces
//! a minimum of two points at construction time; deserialization re-validates.
//!
//! # Wire format
//!
//! EWKB layout (little-endian, SRID 4326):
//!
//! ```text
//! Offset  Size  Content
//!      0     1  Endianness marker: 0x01 (little-endian)
//!      1     4  Geometry type word: 0x20000002 (LineString | SRID flag), LE
//!      5     4  SRID 4326, LE  → [0xE6, 0x10, 0x00, 0x00]
//!      9     4  Number of points (u32 LE)
//!     13  16*n  Points: lon f64 LE, lat f64 LE (16 bytes each)
//! ```

use std::fmt;

use bytes::BytesMut;
use postgres_types::{FromSql, IsNull, ToSql, Type};
use serde::{Deserialize, Serialize};

use crate::geo::{GeoError, GeoPoint, ewkb};

/// An ordered sequence of at least 2 `GeoPoint` values.
///
/// Stored as `GEOGRAPHY(LineString, 4326)` in Postgres.
///
/// # Invariant
///
/// `points.len() >= 2`. The constructor enforces this; deserialization
/// re-validates so a value in memory always satisfies the constraint.
///
/// # Display
///
/// `Display` emits `LINESTRING(<lon> <lat>, ...)` per the OGC WKT convention
/// (longitude first within each coordinate pair).
///
/// # Serde
///
/// Serializes as a JSON array of `{"lat": f64, "lon": f64}` objects —
/// the same shape that `GeoPoint` serializes to. Deserialization validates
/// via `LineString::new`, so a list with fewer than two valid points yields a
/// `serde` error.
#[derive(Debug, Clone, PartialEq)]
pub struct LineString {
    pub(crate) points: Vec<GeoPoint>,
}

impl LineString {
    /// Construct a `LineString` from a slice of points.
    ///
    /// Returns `Err(GeoError::InvalidLineString)` if fewer than 2 points are
    /// supplied. Each `GeoPoint` is assumed to be valid (already constructed
    /// via `GeoPoint::new`).
    pub fn new(points: &[GeoPoint]) -> Result<Self, GeoError> {
        if points.len() < 2 {
            return Err(GeoError::InvalidLineString {
                got: points.len(),
                need: 2,
            });
        }
        Ok(Self {
            points: points.to_vec(),
        })
    }

    /// Return a slice over the constituent points.
    pub fn points(&self) -> &[GeoPoint] {
        &self.points
    }

    /// Encode this `LineString` as an EWKB buffer for `GEOGRAPHY(LineString, 4326)`.
    pub fn to_ewkb_bytes(&self) -> Vec<u8> {
        ewkb::encode_linestring(self)
    }

    /// Decode an EWKB buffer into a `LineString`.
    pub fn from_ewkb_bytes(bytes: &[u8]) -> Result<Self, GeoError> {
        ewkb::decode_linestring(bytes)
    }
}

// ── Display (WKT) ─────────────────────────────────────────────────────────────

impl fmt::Display for LineString {
    /// Emit `LINESTRING(<lon> <lat>, <lon> <lat>, ...)` per OGC WKT.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LINESTRING(")?;
        for (i, p) in self.points.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{} {}", p.lon, p.lat)?;
        }
        f.write_str(")")
    }
}

// ── Serde: Serialize as bare array, Deserialize with validation ───────────────

impl Serialize for LineString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.points.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for LineString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let points: Vec<GeoPoint> = Vec::deserialize(deserializer)?;
        LineString::new(&points).map_err(serde::de::Error::custom)
    }
}

// ── postgres_types codec ──────────────────────────────────────────────────────

impl ToSql for LineString {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        out.extend_from_slice(&self.to_ewkb_bytes());
        Ok(IsNull::No)
    }

    /// Accept the Postgres `geography` type.
    fn accepts(ty: &Type) -> bool {
        ty.name() == "geography"
    }

    postgres_types::to_sql_checked!();
}

impl<'a> FromSql<'a> for LineString {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        LineString::from_ewkb_bytes(raw).map_err(|e| Box::new(e) as _)
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

    fn p(lat: f64, lon: f64) -> GeoPoint {
        GeoPoint::new(lat, lon).unwrap()
    }

    #[test]
    fn new_with_zero_points_errors() {
        assert!(matches!(
            LineString::new(&[]),
            Err(GeoError::InvalidLineString { got: 0, need: 2 })
        ));
    }

    #[test]
    fn new_with_one_point_errors() {
        assert!(matches!(
            LineString::new(&[p(0.0, 0.0)]),
            Err(GeoError::InvalidLineString { got: 1, need: 2 })
        ));
    }

    #[test]
    fn new_with_two_points_succeeds() {
        let ls = LineString::new(&[p(0.0, 0.0), p(1.0, 1.0)]).unwrap();
        assert_eq!(ls.points().len(), 2);
    }

    #[test]
    fn new_with_many_points_succeeds() {
        let pts: Vec<GeoPoint> = (0..5).map(|i| p(i as f64, i as f64)).collect();
        let ls = LineString::new(&pts).unwrap();
        assert_eq!(ls.points().len(), 5);
    }

    #[test]
    fn ewkb_round_trip() {
        let ls = LineString::new(&[p(37.7749, -122.4194), p(40.7128, -74.0060)]).unwrap();
        let bytes = ls.to_ewkb_bytes();
        let decoded = LineString::from_ewkb_bytes(&bytes).unwrap();
        assert_eq!(decoded.points().len(), ls.points().len());
        for (a, b) in ls.points().iter().zip(decoded.points().iter()) {
            assert!((a.lat - b.lat).abs() < 1e-9);
            assert!((a.lon - b.lon).abs() < 1e-9);
        }
    }

    #[test]
    fn display_matches_wkt() {
        let ls = LineString::new(&[p(0.0, 0.0), p(0.0, 1.0)]).unwrap();
        assert_eq!(format!("{ls}"), "LINESTRING(0 0, 1 0)");
    }

    #[test]
    fn serde_json_round_trip() {
        let original = LineString::new(&[p(37.7749, -122.4194), p(40.7128, -74.0060)]).unwrap();
        let json = serde_json::to_string(&original).unwrap();
        let decoded: LineString = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn serde_json_rejects_single_point() {
        // Build a JSON array with one valid GeoPoint.
        let json = r#"[{"lat":0.0,"lon":0.0}]"#;
        assert!(serde_json::from_str::<LineString>(json).is_err());
    }

    #[test]
    fn to_sql_writes_ewkb_bytes() {
        use postgres_types::Type;

        let ls = LineString::new(&[p(51.5074, -0.1278), p(48.8566, 2.3522)]).unwrap();
        let expected = ls.to_ewkb_bytes();
        let mut out = BytesMut::new();
        ls.to_sql(&Type::BYTEA, &mut out).unwrap();
        assert_eq!(out.as_ref(), expected.as_slice());
    }
}
