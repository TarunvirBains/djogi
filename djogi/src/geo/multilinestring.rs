//! `MultiLineString` — a collection of one or more `LineString` values.
//! Stored as `GEOGRAPHY(MultiLineString, 4326)` in Postgres. The type enforces
//! a minimum of one linestring at construction time; deserialization
//! re-validates.
//! # Wire format
//! EWKB layout (little-endian, SRID 4326):
//! ```text
//! Offset Size Content
//!  0  1 Endianness marker: 0x01 (little-endian)
//!  1  4 Geometry type word: 0x20000005 (MultiLineString | SRID flag), LE
//!  5  4 SRID 4326, LE → [0xE6, 0x10, 0x00, 0x00]
//!  9  4 Number of sub-linestrings (u32 LE)
//!  13 var Sub-linestrings: each is a headerless EWKB linestring —
//!    [endian_byte(1), ls_type_word_no_srid(4), point_count(4), points...]
//! ```
//! Each sub-linestring carries its own endianness byte and type word
//! (`0x00000002`, no SRID flag) but no SRID — the outer container holds it.
//! Parallels the `MultiPoint` / `MultiPolygon` envelope discipline so a single
//! reading of the family pattern covers all three.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::geo::{GeoError, LineString, ewkb};

/// A collection of at least 1 `LineString`.
/// Stored as `GEOGRAPHY(MultiLineString, 4326)` in Postgres.
/// # Display
/// `Display` emits `MULTILINESTRING((<lon> <lat>,...),...)` per OGC WKT.
/// # Serde
/// Serializes as an array of linestrings (each linestring is itself an array
/// of `{"lat": f64, "lon": f64}` objects). Deserialization validates via
/// `MultiLineString::new`, so an empty array yields a `serde` error.
#[derive(Debug, Clone, PartialEq)]
pub struct MultiLineString {
    pub(crate) lines: Vec<LineString>,
}

impl MultiLineString {
    /// Construct a `MultiLineString` from a `Vec` of `LineString` values.
    /// Returns `Err(GeoError::InvalidMultiLineString)` if `lines` is empty.
    pub fn new(lines: Vec<LineString>) -> Result<Self, GeoError> {
        if lines.is_empty() {
            return Err(GeoError::InvalidMultiLineString {
                reason: "need at least 1 linestring",
            });
        }
        Ok(Self { lines })
    }

    /// Return a slice over the constituent linestrings.
    pub fn lines(&self) -> &[LineString] {
        &self.lines
    }

    /// Encode this `MultiLineString` as an EWKB buffer for
    /// `GEOGRAPHY(MultiLineString, 4326)`.
    pub fn to_ewkb_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(ewkb::multilinestring_byte_len(self));
        ewkb::encode_multilinestring_into(self, &mut buf);
        buf
    }

    /// Decode an EWKB buffer into a `MultiLineString`.
    pub fn from_ewkb_bytes(bytes: &[u8]) -> Result<Self, GeoError> {
        ewkb::decode_multilinestring(bytes)
    }
}

// ── Display (WKT) ─────────────────────────────────────────────────────────────

impl fmt::Display for MultiLineString {
    /// Emit `MULTILINESTRING((<lon> <lat>,...),...)` per OGC WKT.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MULTILINESTRING(")?;
        for (i, ls) in self.lines.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            f.write_str("(")?;
            for (j, p) in ls.points().iter().enumerate() {
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

// ── Serde: Serialize as array of linestrings, Deserialize with validation ─────

impl Serialize for MultiLineString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.lines.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for MultiLineString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let lines: Vec<LineString> = Vec::deserialize(deserializer)?;
        MultiLineString::new(lines).map_err(serde::de::Error::custom)
    }
}

// ── postgres_types codec (macro-generated) ───────────────────────────────────

crate::geo::impl_geography_codec!(MultiLineString, ewkb::encode_multilinestring_into);

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "spatial"))]
mod tests {
    use super::*;
    use crate::geo::{GeoError, GeoPoint};

    fn p(lat: f64, lon: f64) -> GeoPoint {
        GeoPoint::new(lat, lon).unwrap()
    }

    fn segment_a() -> LineString {
        LineString::new(&[p(0.0, 0.0), p(0.0, 1.0)]).unwrap()
    }

    fn segment_b() -> LineString {
        LineString::new(&[p(1.0, 0.0), p(1.0, 1.0), p(2.0, 1.0)]).unwrap()
    }

    #[test]
    fn new_with_empty_vec_errors() {
        assert!(matches!(
            MultiLineString::new(vec![]),
            Err(GeoError::InvalidMultiLineString { .. })
        ));
    }

    #[test]
    fn new_with_one_linestring_succeeds() {
        let mls = MultiLineString::new(vec![segment_a()]).unwrap();
        assert_eq!(mls.lines().len(), 1);
    }

    #[test]
    fn new_with_multiple_linestrings_succeeds() {
        let mls = MultiLineString::new(vec![segment_a(), segment_b()]).unwrap();
        assert_eq!(mls.lines().len(), 2);
    }

    #[test]
    fn ewkb_round_trip() {
        let mls = MultiLineString::new(vec![segment_a(), segment_b()]).unwrap();
        let bytes = mls.to_ewkb_bytes();
        let decoded = MultiLineString::from_ewkb_bytes(&bytes).unwrap();
        assert_eq!(decoded.lines().len(), mls.lines().len());
        for (a, b) in mls.lines().iter().zip(decoded.lines().iter()) {
            assert_eq!(a.points().len(), b.points().len());
            for (pa, pb) in a.points().iter().zip(b.points().iter()) {
                assert!((pa.lat - pb.lat).abs() < 1e-9);
                assert!((pa.lon - pb.lon).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn display_matches_wkt_prefix() {
        let mls = MultiLineString::new(vec![segment_a()]).unwrap();
        let s = format!("{mls}");
        assert!(s.starts_with("MULTILINESTRING("));
    }

    #[test]
    fn serde_json_round_trip() {
        let original = MultiLineString::new(vec![segment_a(), segment_b()]).unwrap();
        let json = serde_json::to_string(&original).unwrap();
        let decoded: MultiLineString = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn serde_json_rejects_empty_array() {
        let json = r#"[]"#;
        assert!(serde_json::from_str::<MultiLineString>(json).is_err());
    }
}
