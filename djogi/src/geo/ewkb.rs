//! EWKB encode and decode helpers for `GEOGRAPHY(Point, 4326)`.
//!
//! The wire format is the 25-byte Extended Well-Known Binary (EWKB)
//! little-endian layout that PostGIS uses for `GEOGRAPHY(Point, 4326)`:
//!
//! ```text
//! Offset  Size  Content
//!      0     1  Endianness marker: 0x01 (little-endian)
//!      1     4  Geometry type word: 0x20000001 (Point | SRID flag)
//!                 little-endian → bytes [0x01, 0x00, 0x00, 0x20]
//!      5     4  SRID 4326, little-endian
//!                 → bytes [0xE6, 0x10, 0x00, 0x00]
//!      9     8  X coordinate (longitude), f64 little-endian
//!     17     8  Y coordinate (latitude),  f64 little-endian
//! ```
//!
//! Total wire length: 1 + 4 + 4 + 8 + 8 = 25 bytes.
//!
//! This module has NO dependency on `postgres_types`, `bytes`, or `serde`.
//! It only imports `GeoError` from the parent module so it can be kept
//! self-contained for a future swap if polygon or multipoint support lands.

use crate::geo::GeoError;

/// Total byte length of an EWKB-encoded 2-D point with SRID.
pub(crate) const EWKB_LEN: usize = 25;

/// Endianness marker byte (little-endian).
const ENDIAN_BYTE: u8 = 0x01;

/// Geometry type word for `Point | SRID_FLAG`, encoded as 4 little-endian
/// bytes. The canonical `u32` value is `0x20000001`.
const TYPE_BYTES: [u8; 4] = [0x01, 0x00, 0x00, 0x20];

/// SRID 4326 (WGS-84) encoded as 4 little-endian bytes.
const SRID_BYTES: [u8; 4] = [0xE6, 0x10, 0x00, 0x00];

/// The SRID value in integer form, for use in validation and error messages.
const SRID_4326: u32 = 4326;

/// Encode `(lon, lat)` as a 25-byte EWKB buffer.
///
/// The caller is responsible for supplying valid, finite coordinates.
/// This function does not re-validate coordinate ranges — that is
/// [`GeoPoint::new`](super::GeoPoint::new)'s responsibility.
pub(crate) fn encode_point(lon: f64, lat: f64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(EWKB_LEN);
    buf.push(ENDIAN_BYTE);
    buf.extend_from_slice(&TYPE_BYTES);
    buf.extend_from_slice(&SRID_BYTES);
    buf.extend_from_slice(&lon.to_le_bytes());
    buf.extend_from_slice(&lat.to_le_bytes());
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

    if bytes[0] != ENDIAN_BYTE {
        return Err(GeoError::MalformedEwkb(format!(
            "expected little-endian marker 0x01, got 0x{:02X}",
            bytes[0]
        )));
    }

    if bytes[1..5] != TYPE_BYTES {
        return Err(GeoError::MalformedEwkb(format!(
            "unexpected geometry type bytes {:?}, expected {:?}",
            &bytes[1..5],
            TYPE_BYTES
        )));
    }

    let srid = u32::from_le_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]);
    if srid != SRID_4326 {
        return Err(GeoError::UnexpectedSrid(srid));
    }

    let lon = f64::from_le_bytes(bytes[9..17].try_into().expect("slice is exactly 8 bytes"));
    let lat = f64::from_le_bytes(bytes[17..25].try_into().expect("slice is exactly 8 bytes"));

    Ok((lon, lat))
}
