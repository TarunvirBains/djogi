//! `MultiPolygon` — a collection of one or more `Polygon` values.
//!
//! Stored as `GEOGRAPHY(MultiPolygon, 4326)` in Postgres. The type enforces
//! a minimum of one polygon at construction time; deserialization re-validates.
//!
//! # Wire format
//!
//! EWKB layout (little-endian, SRID 4326):
//!
//! ```text
//! Offset  Size  Content
//!      0     1  Endianness marker: 0x01 (little-endian)
//!      1     4  Geometry type word: 0x20000006 (MultiPolygon | SRID flag), LE
//!      5     4  SRID 4326, LE  → [0xE6, 0x10, 0x00, 0x00]
//!      9     4  Number of sub-polygons (u32 LE)
//!     13   var  Sub-polygons: each is a headerless EWKB polygon —
//!               [endian_byte(1), poly_type_word_no_srid(4), ring_count(4), rings...]
//! ```
//!
//! Each sub-polygon carries its own endianness byte and type word
//! (`0x00000003`, no SRID flag) but no SRID — the outer container holds it.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::geo::{GeoError, Polygon, ewkb};

/// A collection of at least 1 `Polygon`.
///
/// Stored as `GEOGRAPHY(MultiPolygon, 4326)` in Postgres.
///
/// # Display
///
/// `Display` emits `MULTIPOLYGON(((...)), ...)` per OGC WKT.
///
/// # Serde
///
/// Serializes as an array of polygons (each polygon is itself an array of
/// rings, each ring an array of `{"lat": f64, "lon": f64}` objects).
/// Deserialization validates via `MultiPolygon::new`, so an empty array yields
/// a `serde` error.
#[derive(Debug, Clone, PartialEq)]
pub struct MultiPolygon {
    pub(crate) polygons: Vec<Polygon>,
}

impl MultiPolygon {
    /// Construct a `MultiPolygon` from a `Vec` of `Polygon` values.
    ///
    /// Returns `Err(GeoError::InvalidMultiPolygon)` if `polygons` is empty.
    pub fn new(polygons: Vec<Polygon>) -> Result<Self, GeoError> {
        if polygons.is_empty() {
            return Err(GeoError::InvalidMultiPolygon {
                reason: "need at least 1 polygon",
            });
        }
        Ok(Self { polygons })
    }

    /// Return a slice over the constituent polygons.
    pub fn polygons(&self) -> &[Polygon] {
        &self.polygons
    }

    /// Encode this `MultiPolygon` as an EWKB buffer for `GEOGRAPHY(MultiPolygon, 4326)`.
    pub fn to_ewkb_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(ewkb::multipolygon_byte_len(self));
        ewkb::encode_multipolygon_into(self, &mut buf);
        buf
    }

    /// Decode an EWKB buffer into a `MultiPolygon`.
    pub fn from_ewkb_bytes(bytes: &[u8]) -> Result<Self, GeoError> {
        ewkb::decode_multipolygon(bytes)
    }
}

// ── Display (WKT) ─────────────────────────────────────────────────────────────

impl fmt::Display for MultiPolygon {
    /// Emit `MULTIPOLYGON(((<lon> <lat>, ...)), ...)` per OGC WKT.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MULTIPOLYGON(")?;
        for (i, poly) in self.polygons.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            f.write_str("(")?;
            for (ri, ring) in poly.rings().iter().enumerate() {
                if ri > 0 {
                    f.write_str(", ")?;
                }
                f.write_str("(")?;
                for (j, p) in ring.iter().enumerate() {
                    if j > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{} {}", p.lon, p.lat)?;
                }
                f.write_str(")")?;
            }
            f.write_str(")")?;
        }
        f.write_str(")")
    }
}

// ── Serde: Serialize as array of polygons, Deserialize with validation ────────

impl Serialize for MultiPolygon {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.polygons.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for MultiPolygon {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let polygons: Vec<Polygon> = Vec::deserialize(deserializer)?;
        MultiPolygon::new(polygons).map_err(serde::de::Error::custom)
    }
}

// ── postgres_types codec (macro-generated) ───────────────────────────────────

crate::geo::impl_geography_codec!(MultiPolygon, ewkb::encode_multipolygon_into);

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "spatial"))]
mod tests {
    use super::*;
    use crate::geo::{GeoError, GeoPoint};

    fn p(lat: f64, lon: f64) -> GeoPoint {
        GeoPoint::new(lat, lon).unwrap()
    }

    fn square() -> Polygon {
        Polygon::with_ring(vec![p(0.0, 0.0), p(0.0, 1.0), p(1.0, 1.0), p(0.0, 0.0)]).unwrap()
    }

    fn triangle() -> Polygon {
        Polygon::with_ring(vec![p(2.0, 0.0), p(2.0, 1.0), p(3.0, 0.5), p(2.0, 0.0)]).unwrap()
    }

    #[test]
    fn new_with_empty_vec_errors() {
        assert!(matches!(
            MultiPolygon::new(vec![]),
            Err(GeoError::InvalidMultiPolygon { .. })
        ));
    }

    #[test]
    fn new_with_one_polygon_succeeds() {
        let mp = MultiPolygon::new(vec![square()]).unwrap();
        assert_eq!(mp.polygons().len(), 1);
    }

    #[test]
    fn new_with_multiple_polygons_succeeds() {
        let mp = MultiPolygon::new(vec![square(), triangle()]).unwrap();
        assert_eq!(mp.polygons().len(), 2);
    }

    #[test]
    fn ewkb_round_trip() {
        let mp = MultiPolygon::new(vec![square(), triangle()]).unwrap();
        let bytes = mp.to_ewkb_bytes();
        let decoded = MultiPolygon::from_ewkb_bytes(&bytes).unwrap();
        assert_eq!(decoded.polygons().len(), mp.polygons().len());
        for (a, b) in mp.polygons().iter().zip(decoded.polygons().iter()) {
            assert_eq!(a.rings().len(), b.rings().len());
            for (ra, rb) in a.rings().iter().zip(b.rings().iter()) {
                assert_eq!(ra.len(), rb.len());
                for (pa, pb) in ra.iter().zip(rb.iter()) {
                    assert!((pa.lat - pb.lat).abs() < 1e-9);
                    assert!((pa.lon - pb.lon).abs() < 1e-9);
                }
            }
        }
    }

    #[test]
    fn display_matches_wkt_prefix() {
        let mp = MultiPolygon::new(vec![square()]).unwrap();
        let s = format!("{mp}");
        assert!(s.starts_with("MULTIPOLYGON("));
    }

    #[test]
    fn serde_json_round_trip() {
        let original = MultiPolygon::new(vec![square()]).unwrap();
        let json = serde_json::to_string(&original).unwrap();
        let decoded: MultiPolygon = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn serde_json_rejects_empty_array() {
        let json = r#"[]"#;
        assert!(serde_json::from_str::<MultiPolygon>(json).is_err());
    }
}
