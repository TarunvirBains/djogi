//! EWKB encode and decode helpers for all Djogi geography types.
//!
//! # Point wire format (25 bytes)
//!
//! ```text
//! Offset  Size  Content
//!      0     1  Endianness marker: 0x01 (little-endian)
//!      1     4  Geometry type word: 0x20000001 (Point | SRID flag), LE
//!      5     4  SRID 4326, LE  → [0xE6, 0x10, 0x00, 0x00]
//!      9     8  X (longitude), f64 LE
//!     17     8  Y (latitude),  f64 LE
//! ```
//!
//! # LineString wire format
//!
//! `[endian(1), type_word 0x20000002 LE(4), srid(4), num_points u32 LE(4), points(16*n)]`
//!
//! # Polygon wire format
//!
//! `[endian(1), type_word 0x20000003 LE(4), srid(4), num_rings u32 LE(4), rings...]`
//! Each ring: `[num_points u32 LE(4), points(16*n)]`.
//!
//! This module has NO dependency on `postgres_types`, `bytes`, or `serde`.

use crate::geo::GeoError;

// ── shared constants ──────────────────────────────────────────────────────────

/// Total byte length of an EWKB-encoded 2-D point with SRID.
pub(crate) const EWKB_LEN: usize = 25;

/// Endianness marker byte (little-endian).
const ENDIAN_BYTE: u8 = 0x01;

/// SRID 4326 (WGS-84) encoded as 4 little-endian bytes.
const SRID_BYTES: [u8; 4] = [0xE6, 0x10, 0x00, 0x00];

/// The SRID value in integer form, for use in validation and error messages.
const SRID_4326: u32 = 4326;

// ── type words (with SRID flag 0x20000000) ────────────────────────────────────

/// Point | SRID_FLAG → `0x20000001` LE.
const TYPE_BYTES: [u8; 4] = [0x01, 0x00, 0x00, 0x20];
/// LineString | SRID_FLAG → `0x20000002` LE.
const TYPE_LINESTRING: [u8; 4] = [0x02, 0x00, 0x00, 0x20];
/// Polygon | SRID_FLAG → `0x20000003` LE.
const TYPE_POLYGON: [u8; 4] = [0x03, 0x00, 0x00, 0x20];

// ── EWKB header helpers ───────────────────────────────────────────────────────

/// Write the standard outer EWKB header: endian byte + type word + SRID bytes.
fn push_outer_header(buf: &mut Vec<u8>, type_bytes: [u8; 4]) {
    buf.push(ENDIAN_BYTE);
    buf.extend_from_slice(&type_bytes);
    buf.extend_from_slice(&SRID_BYTES);
}

/// Validate the outer EWKB header and return the byte offset just after the
/// SRID (i.e., the first byte of payload after the 9-byte header).
///
/// Checks: endian byte, type word, SRID 4326.
fn read_outer_header(bytes: &[u8], expected_type: [u8; 4]) -> Result<usize, GeoError> {
    if bytes.len() < 9 {
        return Err(GeoError::MalformedEwkb(format!(
            "buffer too short: {} bytes (need at least 9 for EWKB header)",
            bytes.len()
        )));
    }
    if bytes[0] != ENDIAN_BYTE {
        return Err(GeoError::MalformedEwkb(format!(
            "expected little-endian marker 0x01, got 0x{:02X}",
            bytes[0]
        )));
    }
    if bytes[1..5] != expected_type {
        return Err(GeoError::MalformedEwkb(format!(
            "unexpected geometry type bytes {:?}, expected {:?}",
            &bytes[1..5],
            expected_type
        )));
    }
    let srid = u32::from_le_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]);
    if srid != SRID_4326 {
        return Err(GeoError::UnexpectedSrid(srid));
    }
    Ok(9)
}

/// Read a `u32` from `bytes[pos..pos+4]` (little-endian).
fn read_u32(bytes: &[u8], pos: usize) -> Result<u32, GeoError> {
    if pos + 4 > bytes.len() {
        return Err(GeoError::MalformedEwkb(format!(
            "buffer too short to read u32 at offset {pos}: only {} bytes total",
            bytes.len()
        )));
    }
    Ok(u32::from_le_bytes([
        bytes[pos],
        bytes[pos + 1],
        bytes[pos + 2],
        bytes[pos + 3],
    ]))
}

/// Read an `f64` from `bytes[pos..pos+8]` (little-endian).
fn read_f64(bytes: &[u8], pos: usize) -> Result<f64, GeoError> {
    if pos + 8 > bytes.len() {
        return Err(GeoError::MalformedEwkb(format!(
            "buffer too short to read f64 at offset {pos}: only {} bytes total",
            bytes.len()
        )));
    }
    Ok(f64::from_le_bytes(
        bytes[pos..pos + 8]
            .try_into()
            .expect("slice is exactly 8 bytes"),
    ))
}

/// Write 16 bytes for a single coordinate pair `(lon, lat)`.
fn push_coord_pair(buf: &mut Vec<u8>, lon: f64, lat: f64) {
    buf.extend_from_slice(&lon.to_le_bytes());
    buf.extend_from_slice(&lat.to_le_bytes());
}

/// Read a coordinate pair at `pos`, returning `(lon, lat, next_pos)`.
fn read_coord_pair(bytes: &[u8], pos: usize) -> Result<(f64, f64, usize), GeoError> {
    let lon = read_f64(bytes, pos)?;
    let lat = read_f64(bytes, pos + 8)?;
    Ok((lon, lat, pos + 16))
}

// ── GeoPoint codec ────────────────────────────────────────────────────────────

/// Encode `(lon, lat)` as a 25-byte EWKB buffer.
///
/// The caller is responsible for supplying valid, finite coordinates.
/// This function does not re-validate coordinate ranges — that is
/// [`GeoPoint::new`](super::GeoPoint::new)'s responsibility.
pub(crate) fn encode_point(lon: f64, lat: f64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(EWKB_LEN);
    push_outer_header(&mut buf, TYPE_BYTES);
    push_coord_pair(&mut buf, lon, lat);
    buf
}

/// Decode a 25-byte EWKB buffer into `(lon, lat)`.
///
/// Validation is applied at every fixed position:
///
/// - Buffer must be exactly 25 bytes.
/// - Byte 0 must be `0x01` (little-endian marker).
/// - Bytes 1..5 must equal the Point-with-SRID type word `[0x01, 0x00, 0x00, 0x20]`.
/// - Bytes 5..9 must encode SRID 4326; if the SRID parses but is not 4326,
///   `GeoError::UnexpectedSrid` carries the actual integer value so callers
///   can produce a meaningful message.
/// - Bytes 9..17 are the X (longitude) `f64`.
/// - Bytes 17..25 are the Y (latitude) `f64`.
pub(crate) fn decode_point(bytes: &[u8]) -> Result<(f64, f64), GeoError> {
    if bytes.len() != EWKB_LEN {
        return Err(GeoError::MalformedEwkb(format!(
            "expected {} bytes, got {}",
            EWKB_LEN,
            bytes.len()
        )));
    }
    let pos = read_outer_header(bytes, TYPE_BYTES)?;
    let (lon, lat, _) = read_coord_pair(bytes, pos)?;
    Ok((lon, lat))
}

// ── LineString codec ──────────────────────────────────────────────────────────

/// Encode a `LineString` as an EWKB buffer for `GEOGRAPHY(LineString, 4326)`.
pub(crate) fn encode_linestring(ls: &super::LineString) -> Vec<u8> {
    let n = ls.points.len();
    let cap = 9 + 4 + 16 * n;
    let mut buf = Vec::with_capacity(cap);
    push_outer_header(&mut buf, TYPE_LINESTRING);
    buf.extend_from_slice(&(n as u32).to_le_bytes());
    for p in &ls.points {
        push_coord_pair(&mut buf, p.lon, p.lat);
    }
    buf
}

/// Decode an EWKB buffer into a `LineString`.
pub(crate) fn decode_linestring(bytes: &[u8]) -> Result<super::LineString, GeoError> {
    let pos = read_outer_header(bytes, TYPE_LINESTRING)?;
    let n = read_u32(bytes, pos)? as usize;
    let mut pos = pos + 4;
    let mut points = Vec::with_capacity(n);
    for _ in 0..n {
        let (lon, lat, next) = read_coord_pair(bytes, pos)?;
        let p = super::GeoPoint::new(lat, lon).map_err(|e| {
            GeoError::MalformedEwkb(format!("invalid coordinate in LineString EWKB: {e}"))
        })?;
        points.push(p);
        pos = next;
    }
    super::LineString::new(&points)
        .map_err(|e| GeoError::MalformedEwkb(format!("decoded LineString failed validation: {e}")))
}

// ── Polygon codec ─────────────────────────────────────────────────────────────

/// Encode a `Polygon` as an EWKB buffer for `GEOGRAPHY(Polygon, 4326)`.
pub(crate) fn encode_polygon(poly: &super::Polygon) -> Vec<u8> {
    let ring_count = poly.rings.len();
    let point_count: usize = poly.rings.iter().map(|r| r.len()).sum();
    let cap = 9 + 4 + ring_count * 4 + point_count * 16;
    let mut buf = Vec::with_capacity(cap);
    push_outer_header(&mut buf, TYPE_POLYGON);
    buf.extend_from_slice(&(ring_count as u32).to_le_bytes());
    for ring in &poly.rings {
        buf.extend_from_slice(&(ring.len() as u32).to_le_bytes());
        for p in ring {
            push_coord_pair(&mut buf, p.lon, p.lat);
        }
    }
    buf
}

/// Decode an EWKB buffer into a `Polygon`.
pub(crate) fn decode_polygon(bytes: &[u8]) -> Result<super::Polygon, GeoError> {
    let pos = read_outer_header(bytes, TYPE_POLYGON)?;
    let ring_count = read_u32(bytes, pos)? as usize;
    let mut pos = pos + 4;
    let mut rings: Vec<Vec<super::GeoPoint>> = Vec::with_capacity(ring_count);
    for _ in 0..ring_count {
        let n = read_u32(bytes, pos)? as usize;
        pos += 4;
        let mut ring = Vec::with_capacity(n);
        for _ in 0..n {
            let (lon, lat, next) = read_coord_pair(bytes, pos)?;
            let p = super::GeoPoint::new(lat, lon).map_err(|e| {
                GeoError::MalformedEwkb(format!("invalid coordinate in Polygon EWKB: {e}"))
            })?;
            ring.push(p);
            pos = next;
        }
        rings.push(ring);
    }
    if rings.is_empty() {
        return Err(GeoError::MalformedEwkb(
            "Polygon EWKB contains zero rings".to_owned(),
        ));
    }
    let mut iter = rings.into_iter();
    let outer = iter.next().expect("checked non-empty");
    let holes: Vec<Vec<super::GeoPoint>> = iter.collect();
    super::Polygon::with_holes(outer, holes)
        .map_err(|e| GeoError::MalformedEwkb(format!("decoded Polygon failed validation: {e}")))
}

// ── MultiPoint codec ──────────────────────────────────────────────────────────

/// Encode a `MultiPoint` as an EWKB buffer for `GEOGRAPHY(MultiPoint, 4326)`.
///
/// Each sub-point is encoded as a headerless EWKB point:
/// `[endian_byte(1), point_type_no_srid(4), lon_f64_LE(8), lat_f64_LE(8)]`
/// — 21 bytes per sub-point. The SRID is carried only by the outer envelope.
pub(crate) fn encode_multipoint(mp: &super::MultiPoint) -> Vec<u8> {
    // Point base type word (no SRID flag) → `0x00000001` LE.
    const SUBTYPE_POINT: [u8; 4] = [0x01, 0x00, 0x00, 0x00];
    // MultiPoint | SRID_FLAG → `0x20000004` LE.
    const TYPE_MULTIPOINT: [u8; 4] = [0x04, 0x00, 0x00, 0x20];

    let n = mp.points.len();
    // outer header = 9 bytes; count = 4 bytes; each sub-point = 1+4+8+8 = 21 bytes.
    let cap = 9 + 4 + 21 * n;
    let mut buf = Vec::with_capacity(cap);
    push_outer_header(&mut buf, TYPE_MULTIPOINT);
    buf.extend_from_slice(&(n as u32).to_le_bytes());
    for p in &mp.points {
        buf.push(ENDIAN_BYTE);
        buf.extend_from_slice(&SUBTYPE_POINT);
        push_coord_pair(&mut buf, p.lon, p.lat);
    }
    buf
}

/// Decode an EWKB buffer into a `MultiPoint`.
pub(crate) fn decode_multipoint(bytes: &[u8]) -> Result<super::MultiPoint, GeoError> {
    const SUBTYPE_POINT: [u8; 4] = [0x01, 0x00, 0x00, 0x00];
    const TYPE_MULTIPOINT: [u8; 4] = [0x04, 0x00, 0x00, 0x20];

    let pos = read_outer_header(bytes, TYPE_MULTIPOINT)?;
    let n = read_u32(bytes, pos)? as usize;
    let mut pos = pos + 4;
    let mut points = Vec::with_capacity(n);
    for i in 0..n {
        // Each sub-point: endian(1) + type_word(4) + lon(8) + lat(8) = 21 bytes.
        if pos + 21 > bytes.len() {
            return Err(GeoError::MalformedEwkb(format!(
                "MultiPoint sub-point {i} truncated at offset {pos}"
            )));
        }
        if bytes[pos] != ENDIAN_BYTE {
            return Err(GeoError::MalformedEwkb(format!(
                "MultiPoint sub-point {i}: expected little-endian marker, got 0x{:02X}",
                bytes[pos]
            )));
        }
        if bytes[pos + 1..pos + 5] != SUBTYPE_POINT {
            return Err(GeoError::MalformedEwkb(format!(
                "MultiPoint sub-point {i}: unexpected type word {:?}",
                &bytes[pos + 1..pos + 5]
            )));
        }
        pos += 5;
        let (lon, lat, next) = read_coord_pair(bytes, pos)?;
        let p = super::GeoPoint::new(lat, lon).map_err(|e| {
            GeoError::MalformedEwkb(format!(
                "invalid coordinate in MultiPoint sub-point {i}: {e}"
            ))
        })?;
        points.push(p);
        pos = next;
    }
    super::MultiPoint::new(&points)
        .map_err(|e| GeoError::MalformedEwkb(format!("decoded MultiPoint failed validation: {e}")))
}
