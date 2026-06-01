//! `MultiPoint` — an unordered collection of one or more `GeoPoint` values.
//! Stored as `GEOGRAPHY(MultiPoint, 4326)` in Postgres. The type enforces a
//! minimum of one point at construction time; deserialization re-validates.
//! # Wire format
//! EWKB layout (little-endian, SRID 4326):
//! ```text
//! Offset  Size  Content
//!      0     1  Endianness marker: 0x01 (little-endian)
//!      1     4  Geometry type word: 0x20000004 (MultiPoint | SRID flag), LE
//!      5     4  SRID 4326, LE  → [0xE6, 0x10, 0x00, 0x00]
//!      9     4  Number of sub-points (u32 LE)
//!     13  21*n  Sub-points: each is a headerless EWKB point —
//!               [endian_byte(1), point_type_word_no_srid(4), lon_f64_LE(8), lat_f64_LE(8)]
//! ```
//! Each sub-point carries its own endianness byte and type word (`0x00000001`,
//! no SRID flag) but no SRID — the outer container holds the SRID for the
//! whole collection.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::geo::{GeoError, GeoPoint, ewkb};

/// An unordered collection of at least 1 `GeoPoint`.
/// Stored as `GEOGRAPHY(MultiPoint, 4326)` in Postgres.
/// # Display
/// `Display` emits `MULTIPOINT((<lon> <lat>), ...)` per OGC WKT.
/// # Serde
/// Serializes as a JSON array of `{"lat": f64, "lon": f64}` objects.
/// Deserialization validates via `MultiPoint::new`, so an empty array yields a
/// `serde` error.
#[derive(Debug, Clone, PartialEq)]
pub struct MultiPoint {
    pub(crate) points: Vec<GeoPoint>,
}

impl MultiPoint {
    /// Construct a `MultiPoint` from a slice of points.
    /// Returns `Err(GeoError::InvalidMultiPoint)` if `points` is empty.
    pub fn new(points: &[GeoPoint]) -> Result<Self, GeoError> {
        if points.is_empty() {
            return Err(GeoError::InvalidMultiPoint {
                reason: "need at least 1 point",
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

    /// Encode this `MultiPoint` as an EWKB buffer for `GEOGRAPHY(MultiPoint, 4326)`.
    pub fn to_ewkb_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(ewkb::multipoint_byte_len(self));
        ewkb::encode_multipoint_into(self, &mut buf);
        buf
    }

    /// Decode an EWKB buffer into a `MultiPoint`.
    pub fn from_ewkb_bytes(bytes: &[u8]) -> Result<Self, GeoError> {
        ewkb::decode_multipoint(bytes)
    }
}

// ── Display (WKT) ─────────────────────────────────────────────────────────────

impl fmt::Display for MultiPoint {
    /// Emit `MULTIPOINT((<lon> <lat>), ...)` per OGC WKT.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MULTIPOINT(")?;
        for (i, p) in self.points.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "({} {})", p.lon, p.lat)?;
        }
        f.write_str(")")
    }
}

// ── Serde: Serialize as bare array, Deserialize with validation ───────────────

impl Serialize for MultiPoint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.points.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for MultiPoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let points: Vec<GeoPoint> = Vec::deserialize(deserializer)?;
        MultiPoint::new(&points).map_err(serde::de::Error::custom)
    }
}

// ── postgres_types codec (macro-generated) ───────────────────────────────────

crate::geo::impl_geography_codec!(MultiPoint, ewkb::encode_multipoint_into);

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "spatial"))]
mod tests {
    use super::*;
    use crate::geo::GeoError;

    fn p(lat: f64, lon: f64) -> GeoPoint {
        GeoPoint::new(lat, lon).unwrap()
    }

    #[test]
    fn new_with_empty_slice_errors() {
        assert!(matches!(
            MultiPoint::new(&[]),
            Err(GeoError::InvalidMultiPoint { .. })
        ));
    }

    #[test]
    fn new_with_one_point_succeeds() {
        let mp = MultiPoint::new(&[p(0.0, 0.0)]).unwrap();
        assert_eq!(mp.points().len(), 1);
    }

    #[test]
    fn new_with_many_points_succeeds() {
        let pts: Vec<GeoPoint> = (0..4).map(|i| p(i as f64, i as f64)).collect();
        let mp = MultiPoint::new(&pts).unwrap();
        assert_eq!(mp.points().len(), 4);
    }

    #[test]
    fn ewkb_round_trip() {
        let pts = vec![
            p(37.7749, -122.4194),
            p(40.7128, -74.0060),
            p(51.5074, -0.1278),
        ];
        let mp = MultiPoint::new(&pts).unwrap();
        let bytes = mp.to_ewkb_bytes();
        let decoded = MultiPoint::from_ewkb_bytes(&bytes).unwrap();
        assert_eq!(decoded.points().len(), mp.points().len());
        for (a, b) in mp.points().iter().zip(decoded.points().iter()) {
            assert!((a.lat - b.lat).abs() < 1e-9);
            assert!((a.lon - b.lon).abs() < 1e-9);
        }
    }

    #[test]
    fn display_matches_wkt() {
        let mp = MultiPoint::new(&[p(0.0, 0.0), p(0.0, 1.0)]).unwrap();
        assert_eq!(format!("{mp}"), "MULTIPOINT((0 0), (1 0))");
    }

    #[test]
    fn serde_json_round_trip() {
        let pts = vec![p(37.7749, -122.4194), p(40.7128, -74.0060)];
        let original = MultiPoint::new(&pts).unwrap();
        let json = serde_json::to_string(&original).unwrap();
        let decoded: MultiPoint = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn serde_json_rejects_empty_array() {
        let json = r#"[]"#;
        assert!(serde_json::from_str::<MultiPoint>(json).is_err());
    }
}
