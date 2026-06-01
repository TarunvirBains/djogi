//! `Polygon` — a closed ring with optional holes.
//! Stored as `GEOGRAPHY(Polygon, 4326)` in Postgres. Rings are sequences of
//! `GeoPoint` values where the first and last points are identical (closed).
//! At least 3 distinct vertices (4 total including the closing point) are
//! required per ring.
//! # Wire format
//! EWKB layout (little-endian, SRID 4326):
//! ```text
//! Offset  Size  Content
//!      0     1  Endianness marker: 0x01 (little-endian)
//!      1     4  Geometry type word: 0x20000003 (Polygon | SRID flag), LE
//!      5     4  SRID 4326, LE  → [0xE6, 0x10, 0x00, 0x00]
//!      9     4  Number of rings (u32 LE)
//!     13   var  Rings: each ring = [num_points u32 LE, point_data 16*n bytes]
//! ```
//! The outer ring is first; hole rings follow in order.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::geo::{GeoError, GeoPoint, ewkb};

/// A polygon with an outer ring and optional holes.
/// Stored as `GEOGRAPHY(Polygon, 4326)` in Postgres.
/// # Ring invariants
/// - Every ring must have at least 4 points (3 distinct vertices + closing
/// repeat of the first point).
/// - Every ring must be closed: first point equals last point.
/// - `rings[0]` is the outer boundary; `rings[1..]` are holes.
/// # Constructors
/// Three constructors cover common usage patterns:
/// - [`Polygon::closed`] — outer ring, auto-closed if not already closed.
/// - [`Polygon::with_ring`] — outer ring that must already be closed.
/// - [`Polygon::with_holes`] — outer ring plus one or more hole rings.
/// # Display
/// `Display` emits `POLYGON((<lon> <lat>, ...), ...)` per OGC WKT.
/// # Serde
/// Serializes as an array of rings, each ring an array of
/// `{"lat": f64, "lon": f64}` objects. Deserialization validates each ring.
#[derive(Debug, Clone, PartialEq)]
pub struct Polygon {
    /// `rings[0]` is the outer boundary; `rings[1..]` are holes.
    pub(crate) rings: Vec<Vec<GeoPoint>>,
}

/// Validate a single ring: must be closed and have at least 4 points.
fn validate_ring(ring: &[GeoPoint], label: &'static str) -> Result<(), GeoError> {
    if ring.len() < 4 {
        return Err(GeoError::InvalidPolygon { reason: label });
    }
    if ring[0] != *ring.last().expect("ring len >= 4") {
        return Err(GeoError::InvalidPolygon {
            reason: "ring is not closed (first point != last point)",
        });
    }
    Ok(())
}

impl Polygon {
    /// Construct from an outer ring, auto-closing it if needed.
    /// If `points.len < 3` the constructor returns
    /// `GeoError::InvalidPolygon`. If the first and last points differ the
    /// first point is appended to close the ring. After auto-closing, if the
    /// ring still has fewer than 4 points (i.e., the input had exactly 2
    /// distinct points), the constructor returns `GeoError::InvalidPolygon`.
    pub fn closed(points: &[GeoPoint]) -> Result<Self, GeoError> {
        if points.len() < 3 {
            return Err(GeoError::InvalidPolygon {
                reason: "need at least 3 distinct vertices",
            });
        }
        let mut ring = points.to_vec();
        if ring[0] != *ring.last().expect("len >= 3") {
            ring.push(ring[0]);
        }
        // After auto-close, ring must have >= 4 points.
        validate_ring(&ring, "outer ring must have >= 4 points after closing")?;
        Ok(Self { rings: vec![ring] })
    }

    /// Construct from an already-closed outer ring.
    /// Returns `GeoError::InvalidPolygon` if the ring has fewer than 4 points
    /// or is not closed.
    pub fn with_ring(points: Vec<GeoPoint>) -> Result<Self, GeoError> {
        validate_ring(
            &points,
            "outer ring needs at least 4 points and must be closed",
        )?;
        Ok(Self {
            rings: vec![points],
        })
    }

    /// Construct from a closed outer ring plus zero or more closed hole rings.
    /// Returns `GeoError::InvalidPolygon` if the outer ring or any hole ring
    /// fails validation.
    pub fn with_holes(outer: Vec<GeoPoint>, holes: Vec<Vec<GeoPoint>>) -> Result<Self, GeoError> {
        let mut rings = Vec::with_capacity(1 + holes.len());
        rings.push(outer);
        rings.extend(holes);
        Self::from_rings(rings)
    }

    /// Construct from a pre-built `Vec` of rings. `rings[0]` is the outer
    /// boundary; `rings[1..]` are holes. Each ring must already be closed
    /// (first point equals last) and have at least 4 points.
    /// Returns `GeoError::InvalidPolygon` if `rings` is empty or any ring
    /// fails validation. Used by EWKB decoders and the `serde::Deserialize`
    /// impl to avoid the split-then-rejoin pattern that `with_holes`
    /// otherwise forces on callers that already have a `Vec<Vec<GeoPoint>>`
    /// in hand.
    pub fn from_rings(rings: Vec<Vec<GeoPoint>>) -> Result<Self, GeoError> {
        if rings.is_empty() {
            return Err(GeoError::InvalidPolygon {
                reason: "Polygon needs at least one ring",
            });
        }
        validate_ring(
            &rings[0],
            "outer ring needs at least 4 points and must be closed",
        )?;
        for hole in &rings[1..] {
            validate_ring(hole, "hole ring needs at least 4 points and must be closed")?;
        }
        Ok(Self { rings })
    }

    /// All rings: outer ring at index 0, holes at indices 1 and above.
    pub fn rings(&self) -> &[Vec<GeoPoint>] {
        &self.rings
    }

    /// The outer boundary ring.
    pub fn outer(&self) -> &[GeoPoint] {
        &self.rings[0]
    }

    /// The hole rings (empty if no holes).
    pub fn holes(&self) -> &[Vec<GeoPoint>] {
        &self.rings[1..]
    }

    /// Encode this `Polygon` as an EWKB buffer for `GEOGRAPHY(Polygon, 4326)`.
    pub fn to_ewkb_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(ewkb::polygon_byte_len(self));
        ewkb::encode_polygon_into(self, &mut buf);
        buf
    }

    /// Decode an EWKB buffer into a `Polygon`.
    pub fn from_ewkb_bytes(bytes: &[u8]) -> Result<Self, GeoError> {
        ewkb::decode_polygon(bytes)
    }
}

// ── Display (WKT) ─────────────────────────────────────────────────────────────

impl fmt::Display for Polygon {
    /// Emit `POLYGON((<lon> <lat>, ...), ...)` per OGC WKT.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("POLYGON(")?;
        for (i, ring) in self.rings.iter().enumerate() {
            if i > 0 {
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
        f.write_str(")")
    }
}

// ── Serde: Serialize as nested array of rings, Deserialize with validation ────

impl Serialize for Polygon {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.rings.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Polygon {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let rings: Vec<Vec<GeoPoint>> = Vec::deserialize(deserializer)?;
        Polygon::from_rings(rings).map_err(serde::de::Error::custom)
    }
}

// ── postgres_types codec (macro-generated) ───────────────────────────────────

crate::geo::impl_geography_codec!(Polygon, ewkb::encode_polygon_into);

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "spatial"))]
mod tests {
    use super::*;
    use crate::geo::GeoError;

    fn p(lat: f64, lon: f64) -> GeoPoint {
        GeoPoint::new(lat, lon).unwrap()
    }

    /// A simple 4-point closed square ring.
    fn square_ring() -> Vec<GeoPoint> {
        vec![p(0.0, 0.0), p(0.0, 1.0), p(1.0, 1.0), p(0.0, 0.0)]
    }

    /// A 3-point ring that is NOT yet closed (for use with `closed`).
    fn open_triangle() -> Vec<GeoPoint> {
        vec![p(0.0, 0.0), p(0.0, 1.0), p(1.0, 0.5)]
    }

    // ── closed ──────────────────────────────────────────────────────────────

    #[test]
    fn closed_auto_closes_open_ring() {
        let poly = Polygon::closed(&open_triangle()).unwrap();
        let outer = poly.outer();
        assert_eq!(outer.len(), 4, "auto-close must append the first point");
        assert_eq!(outer[0], outer[outer.len() - 1], "ring must be closed");
    }

    #[test]
    fn closed_accepts_already_closed_ring() {
        let poly = Polygon::closed(&square_ring()).unwrap();
        assert_eq!(poly.outer().len(), 4);
    }

    #[test]
    fn closed_rejects_fewer_than_three_points() {
        assert!(matches!(
            Polygon::closed(&[p(0.0, 0.0), p(1.0, 1.0)]),
            Err(GeoError::InvalidPolygon { .. })
        ));
    }

    // ── with_ring ───────────────────────────────────────────────────────────

    #[test]
    fn with_ring_accepts_valid_closed_ring() {
        let poly = Polygon::with_ring(square_ring()).unwrap();
        assert_eq!(poly.outer().len(), 4);
        assert!(poly.holes().is_empty());
    }

    #[test]
    fn with_ring_rejects_unclosed_ring() {
        assert!(matches!(
            Polygon::with_ring(open_triangle()),
            Err(GeoError::InvalidPolygon { .. })
        ));
    }

    #[test]
    fn with_ring_rejects_too_few_points() {
        // Only 3 points and not closed.
        let short = vec![p(0.0, 0.0), p(1.0, 0.0), p(0.5, 1.0)];
        assert!(matches!(
            Polygon::with_ring(short),
            Err(GeoError::InvalidPolygon { .. })
        ));
    }

    // ── with_holes ──────────────────────────────────────────────────────────

    #[test]
    fn with_holes_accepts_outer_no_holes() {
        let poly = Polygon::with_holes(square_ring(), vec![]).unwrap();
        assert!(poly.holes().is_empty());
    }

    #[test]
    fn with_holes_accepts_outer_and_one_hole() {
        let hole = vec![p(0.1, 0.1), p(0.1, 0.2), p(0.2, 0.2), p(0.1, 0.1)];
        let poly = Polygon::with_holes(square_ring(), vec![hole]).unwrap();
        assert_eq!(poly.holes().len(), 1);
    }

    #[test]
    fn with_holes_rejects_invalid_hole() {
        let bad_hole = vec![p(0.1, 0.1), p(0.2, 0.2)]; // only 2 points
        assert!(matches!(
            Polygon::with_holes(square_ring(), vec![bad_hole]),
            Err(GeoError::InvalidPolygon { .. })
        ));
    }

    // ── EWKB round-trips ──────────────────────────────────────────────────────

    #[test]
    fn ewkb_round_trip_no_holes() {
        let poly = Polygon::with_ring(square_ring()).unwrap();
        let bytes = poly.to_ewkb_bytes();
        let decoded = Polygon::from_ewkb_bytes(&bytes).unwrap();
        assert_eq!(decoded.rings().len(), 1);
        let outer = decoded.outer();
        assert_eq!(outer.len(), 4);
        for (a, b) in poly.outer().iter().zip(outer.iter()) {
            assert!((a.lat - b.lat).abs() < 1e-9);
            assert!((a.lon - b.lon).abs() < 1e-9);
        }
    }

    #[test]
    fn ewkb_round_trip_with_hole() {
        let hole = vec![p(0.1, 0.1), p(0.1, 0.2), p(0.2, 0.2), p(0.1, 0.1)];
        let poly = Polygon::with_holes(square_ring(), vec![hole]).unwrap();
        let bytes = poly.to_ewkb_bytes();
        let decoded = Polygon::from_ewkb_bytes(&bytes).unwrap();
        assert_eq!(decoded.rings().len(), 2);
        assert_eq!(decoded.holes().len(), 1);
    }

    // ── Display ───────────────────────────────────────────────────────────────

    #[test]
    fn display_matches_wkt_no_holes() {
        let poly = Polygon::with_ring(square_ring()).unwrap();
        let s = format!("{poly}");
        assert!(s.starts_with("POLYGON("));
        assert!(s.contains("0 0"));
    }

    // ── Serde ─────────────────────────────────────────────────────────────────

    #[test]
    fn serde_json_round_trip() {
        let poly = Polygon::with_ring(square_ring()).unwrap();
        let json = serde_json::to_string(&poly).unwrap();
        let decoded: Polygon = serde_json::from_str(&json).unwrap();
        assert_eq!(poly, decoded);
    }
}
