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
//! This module depends only on `bytes::BufMut` (for the `*_into` encoders
//! that write directly into the `tokio_postgres` bind buffer) and `std`.
//! It does not pull in `postgres_types`, `serde`, or any higher-level
//! database adapter.

use bytes::BufMut;

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
/// MultiPoint | SRID_FLAG → `0x20000004` LE.
const TYPE_MULTIPOINT: [u8; 4] = [0x04, 0x00, 0x00, 0x20];
/// MultiLineString | SRID_FLAG → `0x20000005` LE.
const TYPE_MULTILINESTRING: [u8; 4] = [0x05, 0x00, 0x00, 0x20];
/// MultiPolygon | SRID_FLAG → `0x20000006` LE.
const TYPE_MULTIPOLYGON: [u8; 4] = [0x06, 0x00, 0x00, 0x20];

// ── sub-geometry type words (no SRID flag — used inside Multi* envelopes) ────

/// Point base type word (no SRID flag) → `0x00000001` LE. Used by every
/// headerless sub-point inside a `MultiPoint` envelope.
const SUBTYPE_POINT: [u8; 4] = [0x01, 0x00, 0x00, 0x00];
/// LineString base type word (no SRID flag) → `0x00000002` LE. Used by every
/// headerless sub-linestring inside a `MultiLineString` envelope.
const SUBTYPE_LINESTRING: [u8; 4] = [0x02, 0x00, 0x00, 0x00];
/// Polygon base type word (no SRID flag) → `0x00000003` LE. Used by every
/// headerless sub-polygon inside a `MultiPolygon` envelope.
const SUBTYPE_POLYGON: [u8; 4] = [0x03, 0x00, 0x00, 0x00];

// ── EWKB header helpers ───────────────────────────────────────────────────────

/// Write the standard outer EWKB header: endian byte + type word + SRID bytes.
fn push_outer_header<B: BufMut + ?Sized>(buf: &mut B, type_bytes: [u8; 4]) {
    buf.put_u8(ENDIAN_BYTE);
    buf.put_slice(&type_bytes);
    buf.put_slice(&SRID_BYTES);
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
fn push_coord_pair<B: BufMut + ?Sized>(buf: &mut B, lon: f64, lat: f64) {
    buf.put_slice(&lon.to_le_bytes());
    buf.put_slice(&lat.to_le_bytes());
}

/// Read a coordinate pair at `pos`, returning `(lon, lat, next_pos)`.
fn read_coord_pair(bytes: &[u8], pos: usize) -> Result<(f64, f64, usize), GeoError> {
    let lon = read_f64(bytes, pos)?;
    let lat = read_f64(bytes, pos + 8)?;
    Ok((lon, lat, pos + 16))
}

// ── GeoPoint codec ────────────────────────────────────────────────────────────

/// Encode a `GeoPoint` as a 25-byte EWKB representation directly into `buf`.
///
/// The caller is responsible for supplying valid, finite coordinates.
/// This function does not re-validate coordinate ranges — that is
/// [`GeoPoint::new`](super::GeoPoint::new)'s responsibility.
///
/// Writes into any `BufMut` (e.g. `Vec<u8>` for the convenience wrapper,
/// `BytesMut` for the `ToSql` codec path) to avoid the intermediate
/// allocation a `Vec<u8>`-returning encoder would force.
pub(crate) fn encode_point_into<B: BufMut + ?Sized>(p: &super::GeoPoint, buf: &mut B) {
    push_outer_header(buf, TYPE_BYTES);
    push_coord_pair(buf, p.lon, p.lat);
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

/// Encode a `LineString` as `GEOGRAPHY(LineString, 4326)` EWKB into `buf`.
pub(crate) fn encode_linestring_into<B: BufMut + ?Sized>(ls: &super::LineString, buf: &mut B) {
    let n = ls.points.len();
    push_outer_header(buf, TYPE_LINESTRING);
    buf.put_slice(&(n as u32).to_le_bytes());
    for p in &ls.points {
        push_coord_pair(buf, p.lon, p.lat);
    }
}

/// Compute the exact EWKB byte length for a `LineString` so wrappers can
/// pre-size their backing buffer.
pub(crate) fn linestring_byte_len(ls: &super::LineString) -> usize {
    9 + 4 + 16 * ls.points.len()
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

/// Encode a `Polygon` as `GEOGRAPHY(Polygon, 4326)` EWKB into `buf`.
pub(crate) fn encode_polygon_into<B: BufMut + ?Sized>(poly: &super::Polygon, buf: &mut B) {
    push_outer_header(buf, TYPE_POLYGON);
    buf.put_slice(&(poly.rings.len() as u32).to_le_bytes());
    for ring in &poly.rings {
        buf.put_slice(&(ring.len() as u32).to_le_bytes());
        for p in ring {
            push_coord_pair(buf, p.lon, p.lat);
        }
    }
}

/// Compute the exact EWKB byte length for a `Polygon`.
pub(crate) fn polygon_byte_len(poly: &super::Polygon) -> usize {
    let point_count: usize = poly.rings.iter().map(|r| r.len()).sum();
    9 + 4 + poly.rings.len() * 4 + point_count * 16
}

/// Decode an EWKB buffer into a `Polygon`.
pub(crate) fn decode_polygon(bytes: &[u8]) -> Result<super::Polygon, GeoError> {
    let pos = read_outer_header(bytes, TYPE_POLYGON)?;
    let (poly, _) = decode_polygon_body(bytes, pos, "Polygon EWKB")?;
    Ok(poly)
}

/// Decode the `[ring_count, rings...]` body of a polygon, starting at `pos`
/// (which points to the first byte of `ring_count`). Returns the decoded
/// `Polygon` and the byte offset immediately after it.
///
/// `err_label` names the enclosing geometry for error messages so a malformed
/// `MultiPolygon` sub-polygon and a malformed top-level `Polygon` produce
/// distinguishable diagnostics.
fn decode_polygon_body(
    bytes: &[u8],
    pos: usize,
    err_label: &str,
) -> Result<(super::Polygon, usize), GeoError> {
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
                GeoError::MalformedEwkb(format!("invalid coordinate in {err_label}: {e}"))
            })?;
            ring.push(p);
            pos = next;
        }
        rings.push(ring);
    }
    let poly = super::Polygon::from_rings(rings).map_err(|e| {
        GeoError::MalformedEwkb(format!("decoded {err_label} failed validation: {e}"))
    })?;
    Ok((poly, pos))
}

// ── MultiPoint codec ──────────────────────────────────────────────────────────

/// Encode a `MultiPoint` as an EWKB buffer for `GEOGRAPHY(MultiPoint, 4326)`.
///
/// Each sub-point is encoded as a headerless EWKB point:
/// `[endian_byte(1), point_type_no_srid(4), lon_f64_LE(8), lat_f64_LE(8)]`
/// — 21 bytes per sub-point. The SRID is carried only by the outer envelope.
pub(crate) fn encode_multipoint_into<B: BufMut + ?Sized>(mp: &super::MultiPoint, buf: &mut B) {
    push_outer_header(buf, TYPE_MULTIPOINT);
    buf.put_slice(&(mp.points.len() as u32).to_le_bytes());
    for p in &mp.points {
        buf.put_u8(ENDIAN_BYTE);
        buf.put_slice(&SUBTYPE_POINT);
        push_coord_pair(buf, p.lon, p.lat);
    }
}

/// Compute the exact EWKB byte length for a `MultiPoint` (each sub-point is
/// 21 bytes: endian(1) + type word(4) + lon(8) + lat(8); the SRID lives in
/// the outer envelope only).
pub(crate) fn multipoint_byte_len(mp: &super::MultiPoint) -> usize {
    9 + 4 + 21 * mp.points.len()
}

/// Decode an EWKB buffer into a `MultiPoint`.
pub(crate) fn decode_multipoint(bytes: &[u8]) -> Result<super::MultiPoint, GeoError> {
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

// ── MultiLineString codec ─────────────────────────────────────────────────────

/// Encode a `MultiLineString` as an EWKB buffer for
/// `GEOGRAPHY(MultiLineString, 4326)`.
///
/// Each sub-linestring is encoded as a headerless EWKB linestring:
/// `[endian_byte(1), ls_type_no_srid(4), point_count(4), points...]`
/// — 9 bytes of header + 16 bytes per coord pair. The SRID is carried only by
/// the outer envelope.
pub(crate) fn encode_multilinestring_into<B: BufMut + ?Sized>(
    mls: &super::MultiLineString,
    buf: &mut B,
) {
    push_outer_header(buf, TYPE_MULTILINESTRING);
    buf.put_slice(&(mls.lines.len() as u32).to_le_bytes());
    for ls in &mls.lines {
        buf.put_u8(ENDIAN_BYTE);
        buf.put_slice(&SUBTYPE_LINESTRING);
        buf.put_slice(&(ls.points.len() as u32).to_le_bytes());
        for p in &ls.points {
            push_coord_pair(buf, p.lon, p.lat);
        }
    }
}

/// Compute the exact EWKB byte length for a `MultiLineString`. Each
/// sub-linestring header is 9 bytes (endian + type word + point count, no SRID).
pub(crate) fn multilinestring_byte_len(mls: &super::MultiLineString) -> usize {
    let mut len = 9 + 4;
    for ls in &mls.lines {
        len += 5 + 4 + ls.points.len() * 16;
    }
    len
}

/// Decode an EWKB buffer into a `MultiLineString`.
pub(crate) fn decode_multilinestring(bytes: &[u8]) -> Result<super::MultiLineString, GeoError> {
    let pos = read_outer_header(bytes, TYPE_MULTILINESTRING)?;
    let line_count = read_u32(bytes, pos)? as usize;
    let mut pos = pos + 4;
    let mut lines = Vec::with_capacity(line_count);
    for li in 0..line_count {
        // Sub-linestring header: endian(1) + type_word(4) = 5 bytes.
        if pos + 5 > bytes.len() {
            return Err(GeoError::MalformedEwkb(format!(
                "MultiLineString sub-linestring {li} header truncated at offset {pos}"
            )));
        }
        if bytes[pos] != ENDIAN_BYTE {
            return Err(GeoError::MalformedEwkb(format!(
                "MultiLineString sub-linestring {li}: expected little-endian marker, got 0x{:02X}",
                bytes[pos]
            )));
        }
        if bytes[pos + 1..pos + 5] != SUBTYPE_LINESTRING {
            return Err(GeoError::MalformedEwkb(format!(
                "MultiLineString sub-linestring {li}: unexpected type word {:?}",
                &bytes[pos + 1..pos + 5]
            )));
        }
        pos += 5;
        // Point count + coords for this sub-linestring.
        let n = read_u32(bytes, pos)? as usize;
        pos += 4;
        let mut points = Vec::with_capacity(n);
        for pi in 0..n {
            let (lon, lat, next) = read_coord_pair(bytes, pos)?;
            let p = super::GeoPoint::new(lat, lon).map_err(|e| {
                GeoError::MalformedEwkb(format!(
                    "invalid coordinate in MultiLineString sub-linestring {li} point {pi}: {e}"
                ))
            })?;
            points.push(p);
            pos = next;
        }
        let ls = super::LineString::new(&points).map_err(|e| {
            GeoError::MalformedEwkb(format!(
                "MultiLineString sub-linestring {li} failed validation: {e}"
            ))
        })?;
        lines.push(ls);
    }
    super::MultiLineString::new(lines).map_err(|e| {
        GeoError::MalformedEwkb(format!("decoded MultiLineString failed validation: {e}"))
    })
}

// ── MultiPolygon codec ────────────────────────────────────────────────────────

/// Encode a `MultiPolygon` as an EWKB buffer for `GEOGRAPHY(MultiPolygon, 4326)`.
///
/// Each sub-polygon is encoded as a headerless EWKB polygon:
/// `[endian_byte(1), poly_type_no_srid(4), ring_count(4), rings...]`
/// The SRID is carried only by the outer envelope.
pub(crate) fn encode_multipolygon_into<B: BufMut + ?Sized>(mp: &super::MultiPolygon, buf: &mut B) {
    push_outer_header(buf, TYPE_MULTIPOLYGON);
    buf.put_slice(&(mp.polygons.len() as u32).to_le_bytes());
    for poly in &mp.polygons {
        buf.put_u8(ENDIAN_BYTE);
        buf.put_slice(&SUBTYPE_POLYGON);
        buf.put_slice(&(poly.rings.len() as u32).to_le_bytes());
        for ring in &poly.rings {
            buf.put_slice(&(ring.len() as u32).to_le_bytes());
            for p in ring {
                push_coord_pair(buf, p.lon, p.lat);
            }
        }
    }
}

/// Compute the exact EWKB byte length for a `MultiPolygon`. Each sub-polygon
/// header is 5 bytes (endian + type word, no SRID).
pub(crate) fn multipolygon_byte_len(mp: &super::MultiPolygon) -> usize {
    let mut len = 9 + 4;
    for poly in &mp.polygons {
        len += 5 + 4 + poly.rings.len() * 4;
        for ring in &poly.rings {
            len += ring.len() * 16;
        }
    }
    len
}

/// Decode an EWKB buffer into a `MultiPolygon`.
pub(crate) fn decode_multipolygon(bytes: &[u8]) -> Result<super::MultiPolygon, GeoError> {
    let pos = read_outer_header(bytes, TYPE_MULTIPOLYGON)?;
    let poly_count = read_u32(bytes, pos)? as usize;
    let mut pos = pos + 4;
    let mut polygons = Vec::with_capacity(poly_count);
    for pi in 0..poly_count {
        // Sub-polygon header: endian(1) + type_word(4) = 5 bytes.
        if pos + 5 > bytes.len() {
            return Err(GeoError::MalformedEwkb(format!(
                "MultiPolygon sub-polygon {pi} header truncated at offset {pos}"
            )));
        }
        if bytes[pos] != ENDIAN_BYTE {
            return Err(GeoError::MalformedEwkb(format!(
                "MultiPolygon sub-polygon {pi}: expected little-endian marker, got 0x{:02X}",
                bytes[pos]
            )));
        }
        if bytes[pos + 1..pos + 5] != SUBTYPE_POLYGON {
            return Err(GeoError::MalformedEwkb(format!(
                "MultiPolygon sub-polygon {pi}: unexpected type word {:?}",
                &bytes[pos + 1..pos + 5]
            )));
        }
        pos += 5;
        let label = format!("MultiPolygon sub-polygon {pi}");
        let (poly, next_pos) = decode_polygon_body(bytes, pos, &label)?;
        polygons.push(poly);
        pos = next_pos;
    }
    super::MultiPolygon::new(polygons).map_err(|e| {
        GeoError::MalformedEwkb(format!("decoded MultiPolygon failed validation: {e}"))
    })
}
