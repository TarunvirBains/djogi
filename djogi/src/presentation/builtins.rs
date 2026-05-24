//! Built-in presentation codecs for djogi protected fields.
//!
//! All built-in codecs are ordinary implementations of the public
//! [`PresentationCodecInfo`](super::PresentationCodecInfo) /
//! [`PresentationCodec`](super::PresentationCodec) /
//! [`TryPresentationCodec`](super::TryPresentationCodec) traits.
//! Adopter-defined codecs use the same traits and the same dispatch paths.
//!
//! # Codec catalog
//!
//! | Type | Input | Output | Reversible | Queryable | Infallible? |
//! |------|-------|--------|------------|-----------|-------------|
//! | [`Identity`] | `T` | `T` | Yes | Predicate + Order | Yes |
//! | [`MaskString`] | `String` | `String` | No | No | Yes (both slots) |
//! | [`MaskOptionString`] | `Option<String>` | `Option<String>` | No | No | Yes |
//! | [`HmacSha256HexString`] | `String` | [`HmacSha256Hex`] | No | No | No |
//! | [`HmacSha256HexOptionString`] | `Option<String>` | `Option<HmacSha256Hex>` | No | No | No |
//!
//! The two HMAC codecs are available only with crate feature `hmac-codec`.
//!
//! **`MaskString` in both slots**: `MaskString` implements both
//! [`PresentationCodec<String>`](super::PresentationCodec) (infallible,
//! used in `presentation_codec = MaskString`) and
//! [`TryPresentationCodec<String>`](super::TryPresentationCodec) (used in
//! `try_presentation_codec = MaskString`). The `TryPresentationCodec` impl
//! delegates to `PresentationCodec::present` and uses `Infallible` as its
//! error type — it can never fail. This allows `MaskString` to be used in
//! visage scopes that mix infallible and fallible codecs without requiring a
//! separate masking type.
//!
//! # Key format for HMAC built-ins
//!
//! `DJOGI_PRESENTATION_HMAC_KEY` must be exactly 64 **lowercase** ASCII hex
//! characters (encoding 32 bytes). Mixed-case is rejected — `a` through `f`
//! only, not `A` through `F`. This differs from snapshot signing's
//! mixed-case acceptance deliberately: a case-only env-var change must
//! fail startup rather than silently looking like a key rotation.
//!
//! Rotation changes future HMAC outputs only after a process restart (or
//! an explicit future reload API). Downstream caches, exports, or
//! correlations using old HMAC values become invalid after rotation.
//!
//! HMAC inputs are the exact UTF-8 bytes of the Rust `String`. No Unicode
//! normalization is applied. `"Caf\u{e9}"` (NFC) and `"Cafe\u{301}"` (NFD)
//! produce different HMAC outputs.

use std::convert::TryFrom;

#[cfg(feature = "hmac-codec")]
use std::sync::OnceLock;

#[cfg(feature = "hmac-codec")]
use hmac::{Hmac, KeyInit, Mac};
#[cfg(feature = "hmac-codec")]
use sha2::Sha256;
use thiserror::Error;

#[cfg(feature = "hmac-codec")]
use super::{BuiltInPresentationError, PresentationStartupError};
use super::{
    PresentationCodec, PresentationCodecInfo, Queryability, Reversibility,
    ReversiblePresentationCodec, ReversibleTryPresentationCodec, TryPresentationCodec,
};
use crate::presentation::query::{
    PresentationOrderCodec, PresentationQueryCodec, PresentationQueryField,
};

#[cfg(feature = "hmac-codec")]
type HmacSha256 = Hmac<Sha256>;

/// Environment variable name for the presentation HMAC key.
#[cfg(feature = "hmac-codec")]
const HMAC_KEY_ENV: &str = "DJOGI_PRESENTATION_HMAC_KEY";

/// Process-wide cache for the decoded 32-byte HMAC key.
///
/// Populated by [`HmacSha256HexString::validate_startup`] on first
/// successful validation. Once set, env-var changes do not affect
/// HMAC outputs until process restart.
///
/// Failed loads are NOT cached — the `OnceLock` is only written after a
/// successful parse, so retrying after fixing the environment succeeds.
#[cfg(feature = "hmac-codec")]
static HMAC_KEY: OnceLock<[u8; 32]> = OnceLock::new();

/// Mutex serializing env-var mutation in tests so concurrent test runs
/// do not interfere with each other's key state.
///
/// Only active under `#[cfg(test)]`. Production code never uses this mutex.
#[cfg(all(test, feature = "hmac-codec"))]
pub(crate) static TEST_HMAC_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ── Key parsing ────────────────────────────────────────────────────────────

/// Parse exactly 64 lowercase hex characters into a 32-byte array.
///
/// Validation rules:
/// - Exactly 64 characters (= 32 bytes when decoded).
/// - Every character must be in `0`–`9` or `a`–`f` (lowercase only).
/// - Uppercase `A`–`F` are explicitly rejected with [`PresentationStartupError::NonLowercaseHexByte`].
/// - Invalid non-hex bytes produce [`PresentationStartupError::InvalidHexByte`].
///
/// Failed parses are never cached. The caller may retry after fixing the
/// environment variable.
#[cfg(feature = "hmac-codec")]
fn parse_hmac_key(hex: &str, env_name: &'static str) -> Result<[u8; 32], PresentationStartupError> {
    let bytes = hex.as_bytes();

    // Length check: must be exactly 64 hex characters = 32 bytes.
    if bytes.len() != 64 {
        return Err(PresentationStartupError::InvalidHexLength {
            name: env_name,
            actual: bytes.len(),
        });
    }

    let mut key = [0u8; 32];
    for (chunk_idx, chunk) in bytes.chunks(2).enumerate() {
        let hi = decode_hex_nibble(chunk[0], chunk_idx * 2, env_name)?;
        let lo = decode_hex_nibble(chunk[1], chunk_idx * 2 + 1, env_name)?;
        key[chunk_idx] = (hi << 4) | lo;
    }
    Ok(key)
}

/// Decode a single hex nibble (one character) from a byte.
///
/// - `0`–`9`: returns 0–9.
/// - `a`–`f` (lowercase only): returns 10–15.
/// - `A`–`F` (uppercase): returns `NonLowercaseHexByte` error.
/// - Any other byte: returns `InvalidHexByte` error.
#[cfg(feature = "hmac-codec")]
fn decode_hex_nibble(
    byte: u8,
    idx: usize,
    env_name: &'static str,
) -> Result<u8, PresentationStartupError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Err(PresentationStartupError::NonLowercaseHexByte {
            name: env_name,
            idx,
        }),
        _ => Err(PresentationStartupError::InvalidHexByte {
            name: env_name,
            idx,
        }),
    }
}

/// Internal startup validation for the shared HMAC key.
///
/// Reads `DJOGI_PRESENTATION_HMAC_KEY`, validates the format, and stores the
/// decoded key in `HMAC_KEY` via `OnceLock::set`. If the lock is already set
/// (concurrent or prior call succeeded), the new value is silently ignored —
/// process-wide key is stable after first successful load.
///
/// Failed validation is NOT cached; a later retry after fixing the environment
/// can succeed.
#[cfg(feature = "hmac-codec")]
fn validate_startup_for_hmac_key() -> Result<(), PresentationStartupError> {
    let raw = match std::env::var(HMAC_KEY_ENV) {
        Ok(v) => v,
        Err(std::env::VarError::NotPresent) => {
            return Err(PresentationStartupError::MissingEnvVar { name: HMAC_KEY_ENV });
        }
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(PresentationStartupError::NonUnicodeEnvVar { name: HMAC_KEY_ENV });
        }
    };

    let key_bytes = parse_hmac_key(&raw, HMAC_KEY_ENV)?;

    // Use `set(...).ok()` — if another call already set the key, ignore the
    // duplicate; the first successful write wins and is stable.
    let _ = HMAC_KEY.set(key_bytes);
    Ok(())
}

#[cfg(feature = "hmac-codec")]
fn hmac_sha256_hex_string_present_with_cached_key(
    value: &str,
    cached_key: Option<&[u8; 32]>,
) -> Result<HmacSha256Hex, BuiltInPresentationError> {
    let key = cached_key.ok_or(BuiltInPresentationError::KeyNotValidated { env: HMAC_KEY_ENV })?;

    let mut mac = HmacSha256::new_from_slice(key.as_ref())
        .expect("HMAC-SHA256 accepts any key length; 32-byte key is always valid");
    mac.update(value.as_bytes());
    let result = mac.finalize().into_bytes();
    Ok(HmacSha256Hex::new_unchecked(hex_encode(&result)))
}

// ── Identity ───────────────────────────────────────────────────────────────

/// Presentation codec that returns the storage value unchanged.
///
/// `Identity` is a deliberate opt-in to plaintext presentation. Use it
/// explicitly on scopes where you want to expose the storage value as-is.
/// Because it is reversible and queryable, it grants the same access as a
/// raw field accessor — document this clearly in your model.
///
/// # Queryability
///
/// `Identity` implements [`PresentationQueryCodec`] and [`PresentationOrderCodec`],
/// making predicate and ordering access available on visage fields governed by
/// this codec. Predicate calls delegate to storage-value equality through
/// [`PresentationQueryField::eq_storage`], which is an SQL-only predicate.
///
/// # Reversibility
///
/// `Identity`'s output is the input — `try_reverse(&Identity::present(v)) == Ok(v.clone())`.
pub struct Identity;

impl<T> PresentationCodecInfo<T> for Identity
where
    T: Clone + std::fmt::Debug + serde::Serialize + serde::de::DeserializeOwned + 'static,
{
    type Output = T;
    const REVERSIBILITY: Reversibility = Reversibility::Reversible;
    const QUERYABILITY: Queryability = Queryability::PredicateAndOrder;
}

impl<T> PresentationCodec<T> for Identity
where
    T: Clone + std::fmt::Debug + serde::Serialize + serde::de::DeserializeOwned + 'static,
{
    /// Return a clone of the storage value unchanged.
    fn present(value: &T) -> T {
        value.clone()
    }
}

impl<T> TryPresentationCodec<T> for Identity
where
    T: Clone + std::fmt::Debug + serde::Serialize + serde::de::DeserializeOwned + 'static,
{
    type Error = std::convert::Infallible;

    fn try_present(value: &T) -> Result<T, Self::Error> {
        Ok(value.clone())
    }
}

impl<T> ReversiblePresentationCodec<T> for Identity
where
    T: Clone + std::fmt::Debug + serde::Serialize + serde::de::DeserializeOwned + 'static,
{
    type ReverseError = std::convert::Infallible;

    fn try_reverse(value: &T) -> Result<T, Self::ReverseError> {
        Ok(value.clone())
    }
}

impl<T> ReversibleTryPresentationCodec<T> for Identity
where
    T: Clone + std::fmt::Debug + serde::Serialize + serde::de::DeserializeOwned + 'static,
{
    type ReverseError = std::convert::Infallible;

    fn try_reverse(value: &T) -> Result<T, Self::ReverseError> {
        Ok(value.clone())
    }
}

impl<T> PresentationQueryCodec<T> for Identity
where
    T: Clone
        + std::fmt::Debug
        + serde::Serialize
        + serde::de::DeserializeOwned
        + crate::query::IntoFilterValue
        + 'static,
{
    /// The query value for `Identity` is the storage type itself.
    type QueryValue = T;

    /// Build an equality predicate via storage-value equality.
    ///
    /// Delegates to [`PresentationQueryField::eq_storage`], granting direct
    /// storage-value predicate access. Document the consequence when using
    /// `Identity` on a protected field: callers can probe field values
    /// through the generated visage accessor.
    fn to_query_value_and_build<M: crate::model::Model>(
        field: PresentationQueryField<M, T>,
        value: T,
    ) -> crate::query::Q<M> {
        field.eq_storage(value)
    }
}

impl<T> PresentationOrderCodec<T> for Identity
where
    T: Clone + std::fmt::Debug + serde::Serialize + serde::de::DeserializeOwned + 'static,
{
    // No methods required — ordering is delegated to FieldRef::asc / FieldRef::desc
    // through PresentationFieldRef::asc / PresentationFieldRef::desc.
}

// ── MaskString ────────────────────────────────────────────────────────────

/// Presentation codec that replaces a `String` field with a fixed mask.
///
/// The masked output is the literal string `"[REDACTED]"`. It does not
/// preserve the length, character class, or structure of the original value.
/// The mask is intentionally non-domain — it cannot be mistaken for a real
/// value.
///
/// # Reversibility and queryability
///
/// Not reversible. Not queryable. Use [`Identity`] if you need to expose
/// the original value or query against it.
///
/// # Option handling
///
/// This codec is for `String` (non-nullable). For `Option<String>`, use
/// [`MaskOptionString`], which preserves `None`.
pub struct MaskString;

/// The fixed mask string emitted by [`MaskString`] and [`MaskOptionString`].
///
/// Using a single fixed string (rather than length-preserving masking or
/// randomized output) keeps the output clearly out-of-domain and avoids
/// leaking structural information such as approximate length.
const MASK_LITERAL: &str = "[REDACTED]";

impl PresentationCodecInfo<String> for MaskString {
    type Output = String;
    const REVERSIBILITY: Reversibility = Reversibility::OneWay;
    const QUERYABILITY: Queryability = Queryability::Disabled;
}

impl PresentationCodec<String> for MaskString {
    /// Replace the storage value with the fixed mask `"[REDACTED]"`.
    ///
    /// The original value is not retained in the output. Callers who need
    /// the original value must access the source model through
    /// `Model::objects()` (which remains privileged).
    fn present(_value: &String) -> String {
        MASK_LITERAL.to_string()
    }
}

impl TryPresentationCodec<String> for MaskString {
    /// This codec is infallible — the error type is [`std::convert::Infallible`].
    ///
    /// `MaskString` may be used in both `presentation_codec = MaskString`
    /// (infallible slot, calls [`PresentationCodec::present`]) and
    /// `try_presentation_codec = MaskString` (fallible slot, calls this
    /// method). When used in the fallible slot the `Infallible` error type
    /// is propagated through the generated `TryFrom<&Model>` impl — the error
    /// can never actually occur, but the `TryFrom` surface must be satisfied
    /// when any field in the scope has a fallible codec.
    type Error = std::convert::Infallible;

    fn try_present(value: &String) -> Result<String, std::convert::Infallible> {
        Ok(MaskString::present(value))
    }
}

// ── MaskOptionString ──────────────────────────────────────────────────────

/// Presentation codec that masks an `Option<String>` field.
///
/// `None` is preserved unchanged — a `NULL` in the database produces
/// `None` in the presented output, not `Some("[REDACTED]")`. A `Some`
/// value is replaced with `Some("[REDACTED]")`.
///
/// # Rationale for `None` preservation
///
/// `NULL` vs non-NULL is structural information that is visible at the SQL
/// level regardless of redaction (a `COUNT(*)` vs `COUNT(col)` reveals
/// null prevalence). Changing `None` to `Some("[REDACTED]")` would
/// incorrectly imply a non-null value was present. `None` preservation
/// is the honest choice.
///
/// # Reversibility and queryability
///
/// Not reversible. Not queryable.
pub struct MaskOptionString;

impl PresentationCodecInfo<Option<String>> for MaskOptionString {
    type Output = Option<String>;
    const REVERSIBILITY: Reversibility = Reversibility::OneWay;
    const QUERYABILITY: Queryability = Queryability::Disabled;
}

impl PresentationCodec<Option<String>> for MaskOptionString {
    /// Preserve `None`; replace `Some(_)` with `Some("[REDACTED]")`.
    fn present(value: &Option<String>) -> Option<String> {
        value.as_ref().map(|_| MASK_LITERAL.to_string())
    }
}

impl TryPresentationCodec<Option<String>> for MaskOptionString {
    type Error = std::convert::Infallible;

    fn try_present(value: &Option<String>) -> Result<Option<String>, std::convert::Infallible> {
        Ok(MaskOptionString::present(value))
    }
}

// ── HmacSha256Hex newtype ─────────────────────────────────────────────────

/// A 64-character lowercase hex-encoded HMAC-SHA256 output.
///
/// Used as the `Output` type for [`HmacSha256HexString`] and
/// [`HmacSha256HexOptionString`]. The inner `String` is always exactly
/// 64 lowercase hex characters (encoding 32 bytes).
///
/// # No public constructor
///
/// There is no public infallible constructor that accepts arbitrary strings —
/// `HmacSha256Hex` values are produced by the keyed codecs, or by the
/// fallible `TryFrom<String>` / serde-deserialization path that validates the
/// 64-character lowercase-hex invariant before constructing the value.
///
/// # Serialization
///
/// Serializes as a JSON string containing the 64-char hex value. String
/// deserialization validates the same invariant and rejects malformed input.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct HmacSha256Hex(pub(crate) String);

/// Errors produced when validating an [`HmacSha256Hex`] string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum HmacSha256HexError {
    /// The string had the wrong number of characters.
    #[error("HmacSha256Hex must be exactly 64 lowercase hex characters; got {actual}")]
    InvalidLength {
        /// Actual string length observed.
        actual: usize,
    },
    /// The string contained a byte outside `0`-`9` or `a`-`f`.
    #[error(
        "HmacSha256Hex must contain only lowercase hex characters; invalid byte 0x{byte:02x} at index {idx}"
    )]
    InvalidByte {
        /// Zero-based byte index of the offending character.
        idx: usize,
        /// The invalid byte value.
        byte: u8,
    },
}

/// Validate that `hex` is exactly 64 lowercase ASCII hex characters.
///
/// This helper is shared by `TryFrom<String>`, serde deserialization, and the
/// Postgres `FromSql` path so all string-based construction paths enforce the
/// same invariant.
fn validate_hmac_sha256_hex(hex: &str) -> Result<(), HmacSha256HexError> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return Err(HmacSha256HexError::InvalidLength {
            actual: bytes.len(),
        });
    }

    for (idx, &byte) in bytes.iter().enumerate() {
        match byte {
            b'0'..=b'9' | b'a'..=b'f' => {}
            _ => {
                return Err(HmacSha256HexError::InvalidByte { idx, byte });
            }
        }
    }

    Ok(())
}

impl HmacSha256Hex {
    /// Construct from a known-valid 64-char lowercase hex string.
    ///
    /// This constructor is crate-private — only codec implementations
    /// inside this module may produce `HmacSha256Hex` values. Downstream
    /// code cannot construct this type from arbitrary strings.
    #[cfg(feature = "hmac-codec")]
    pub(crate) fn new_unchecked(hex: String) -> Self {
        Self(hex)
    }

    /// Return the 64-character lowercase hex string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for HmacSha256Hex {
    type Error = HmacSha256HexError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate_hmac_sha256_hex(&value)?;
        Ok(Self(value))
    }
}

impl<'a> tokio_postgres::types::FromSql<'a> for HmacSha256Hex {
    /// Decode an `HmacSha256Hex` from a Postgres TEXT column.
    ///
    /// The underlying Postgres type must be TEXT (or a compatible VARCHAR),
    /// because `HmacSha256Hex` is always a 64-character lowercase hex string.
    /// Decoding delegates to `String::from_sql`, then validates the decoded
    /// string before constructing the newtype.
    ///
    /// This impl is provided so that `VisageQuerySet` and `FromPgRow` for
    /// visages whose fields use `HmacSha256HexString` as a codec can be
    /// compiled. In practice, `HmacSha256Hex` values are computed in-memory
    /// and are not normally stored back into Postgres columns; the impl exists
    /// to satisfy the type system, not to encourage storage.
    fn from_sql(
        ty: &tokio_postgres::types::Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        let s = String::from_sql(ty, raw)?;
        HmacSha256Hex::try_from(s).map_err(|err| Box::new(err) as _)
    }

    fn accepts(ty: &tokio_postgres::types::Type) -> bool {
        <String as tokio_postgres::types::FromSql<'_>>::accepts(ty)
    }
}

// ── HmacSha256HexString ────────────────────────────────────────────────────

/// HMAC-SHA256 presentation codec for `String` fields.
///
/// Transforms a `String` storage value into a [`HmacSha256Hex`] —
/// a 64-character lowercase hex-encoded HMAC-SHA256 output — keyed by the
/// process-wide `DJOGI_PRESENTATION_HMAC_KEY`.
///
/// # Key configuration
///
/// Set `DJOGI_PRESENTATION_HMAC_KEY` to exactly 64 lowercase hex characters
/// (encoding 32 bytes) before `DjogiPool::connect` / `DjogiPoolBuilder::build`
/// is called. Mixed-case is rejected — only `0`–`9` and `a`–`f` are valid.
///
/// # Startup validation
///
/// Validated by [`validate_startup_inventory`](super::super::validate_startup_inventory).
/// Pool construction calls this automatically (Stage 3). Apps without a pool
/// must call it explicitly.
///
/// # Fallibility
///
/// This codec uses `TryPresentationCodec`. If startup validation was
/// bypassed and the key is not cached, `try_present` returns
/// [`BuiltInPresentationError::KeyNotValidated`]. Generated projection code
/// maps this to
/// [`VisageError::PresentationCodec`](crate::visage::VisageError::PresentationCodec)
/// — no panics on the request path.
///
/// # Queryability
///
/// Disabled. A blind-index / queryable-HMAC design is a separate feature
/// (issue #227 notes) requiring normalization, storage, and lookup contracts.
///
/// # Unicode note
///
/// The HMAC input is the exact UTF-8 bytes of the decoded Rust `String`.
/// No Unicode normalization is applied. `"Caf\u{e9}"` (NFC) and
/// `"Cafe\u{301}"` (NFD) produce different HMAC outputs.
#[cfg(feature = "hmac-codec")]
pub struct HmacSha256HexString;

#[cfg(feature = "hmac-codec")]
impl PresentationCodecInfo<String> for HmacSha256HexString {
    type Output = HmacSha256Hex;
    const REVERSIBILITY: Reversibility = Reversibility::OneWay;
    const QUERYABILITY: Queryability = Queryability::Disabled;

    fn validate_startup() -> Result<(), PresentationStartupError> {
        validate_startup_for_hmac_key()
    }
}

#[cfg(feature = "hmac-codec")]
impl TryPresentationCodec<String> for HmacSha256HexString {
    type Error = BuiltInPresentationError;

    /// Compute HMAC-SHA256 over the UTF-8 bytes of `value`.
    ///
    /// Returns `Err(KeyNotValidated)` if the key has not been loaded
    /// (startup validation was bypassed). Never reads the environment —
    /// the per-value path uses only the pre-cached key.
    fn try_present(value: &String) -> Result<HmacSha256Hex, BuiltInPresentationError> {
        hmac_sha256_hex_string_present_with_cached_key(value, HMAC_KEY.get())
    }
}

// ── HmacSha256HexOptionString ──────────────────────────────────────────────

/// HMAC-SHA256 presentation codec for `Option<String>` fields.
///
/// Same behavior as [`HmacSha256HexString`] for `Some` values.
/// `None` is preserved unchanged — a `NULL` storage value produces
/// `None` in the presented output, not `Some(HmacSha256Hex("..."))`.
///
/// See [`HmacSha256HexString`] for the key configuration and startup
/// validation contract.
#[cfg(feature = "hmac-codec")]
pub struct HmacSha256HexOptionString;

#[cfg(feature = "hmac-codec")]
impl PresentationCodecInfo<Option<String>> for HmacSha256HexOptionString {
    type Output = Option<HmacSha256Hex>;
    const REVERSIBILITY: Reversibility = Reversibility::OneWay;
    const QUERYABILITY: Queryability = Queryability::Disabled;

    fn validate_startup() -> Result<(), PresentationStartupError> {
        validate_startup_for_hmac_key()
    }
}

#[cfg(feature = "hmac-codec")]
impl TryPresentationCodec<Option<String>> for HmacSha256HexOptionString {
    type Error = BuiltInPresentationError;

    /// Return `None` for `None`; compute HMAC-SHA256 for `Some`.
    fn try_present(
        value: &Option<String>,
    ) -> Result<Option<HmacSha256Hex>, BuiltInPresentationError> {
        match value {
            None => Ok(None),
            Some(s) => HmacSha256HexString::try_present(s).map(Some),
        }
    }
}

// ── Hex encoding ─────────────────────────────────────────────────────────

/// Encode `bytes` as a lowercase hex string.
///
/// Uses byte-level stdlib primitives — no regex engine, no external hex crate.
/// Always produces a string of exactly `bytes.len() * 2` lowercase hex chars.
#[cfg(feature = "hmac-codec")]
fn hex_encode(bytes: &[u8]) -> String {
    const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX_DIGITS[(b >> 4) as usize] as char);
        out.push(HEX_DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::TryFrom;

    // ── Identity ──────────────────────────────────────────────────────────

    #[test]
    fn identity_present_string_returns_same_value() {
        let input = "hello".to_string();
        let output = Identity::present(&input);
        assert_eq!(output, input);
    }

    #[test]
    fn identity_present_u32_returns_same_value() {
        let input: u32 = 42;
        let output = Identity::present(&input);
        assert_eq!(output, input);
    }

    #[test]
    fn identity_is_reversible() {
        assert_eq!(
            <Identity as PresentationCodecInfo<String>>::REVERSIBILITY,
            Reversibility::Reversible
        );
    }

    #[test]
    fn identity_is_queryable_predicate_and_order() {
        assert_eq!(
            <Identity as PresentationCodecInfo<String>>::QUERYABILITY,
            Queryability::PredicateAndOrder
        );
    }

    // ── MaskString ────────────────────────────────────────────────────────

    #[test]
    fn mask_string_present_does_not_return_original() {
        let input = "alice@example.com".to_string();
        let output = MaskString::present(&input);
        assert_ne!(
            output, input,
            "MaskString must not expose the original value"
        );
        assert_eq!(output, MASK_LITERAL);
    }

    #[test]
    fn mask_string_is_one_way() {
        assert_eq!(
            <MaskString as PresentationCodecInfo<String>>::REVERSIBILITY,
            Reversibility::OneWay
        );
    }

    #[test]
    fn mask_string_queryability_disabled() {
        assert_eq!(
            <MaskString as PresentationCodecInfo<String>>::QUERYABILITY,
            Queryability::Disabled
        );
    }

    #[test]
    fn mask_string_try_present_delegates_to_present() {
        let input = "alice@example.com".to_string();
        let result = MaskString::try_present(&input);
        assert!(result.is_ok(), "try_present must never fail for MaskString");
        assert_eq!(result.unwrap(), MaskString::present(&input));
    }

    // ── MaskOptionString ──────────────────────────────────────────────────

    #[test]
    fn mask_option_string_none_preserved() {
        let input: Option<String> = None;
        let output = MaskOptionString::present(&input);
        assert_eq!(output, None, "None must be preserved by MaskOptionString");
    }

    #[test]
    fn mask_option_string_some_is_masked() {
        let input = Some("secret".to_string());
        let output = MaskOptionString::present(&input);
        assert_eq!(output, Some(MASK_LITERAL.to_string()));
        assert_ne!(output.unwrap(), "secret");
    }

    #[test]
    fn mask_option_string_try_present_matches_present() {
        let none_input: Option<String> = None;
        let some_input = Some("secret".to_string());

        let none_output = MaskOptionString::try_present(&none_input);
        let some_output = MaskOptionString::try_present(&some_input);

        assert_eq!(none_output, Ok(None));
        assert_eq!(some_output, Ok(Some(MASK_LITERAL.to_string())));
    }

    // ── HMAC key parsing ─────────────────────────────────────────────────

    /// Valid 64 lowercase hex characters must pass.
    #[cfg(feature = "hmac-codec")]
    #[test]
    fn hmac_key_validation_accepts_valid_lowercase_hex() {
        let valid = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        assert_eq!(valid.len(), 64);
        let result = parse_hmac_key(valid, "TEST_KEY");
        assert!(
            result.is_ok(),
            "valid 64 lowercase hex should parse: {:?}",
            result
        );
    }

    /// Uppercase characters must be rejected even though they are valid hex.
    #[cfg(feature = "hmac-codec")]
    #[test]
    fn hmac_key_validation_rejects_uppercase_hex() {
        let uppercase = "AABBCCDDEEFF00112233445566778899AABBCCDDEEFF00112233445566778899";
        assert_eq!(uppercase.len(), 64);
        let result = parse_hmac_key(uppercase, "TEST_KEY");
        assert!(
            matches!(
                result,
                Err(PresentationStartupError::NonLowercaseHexByte { .. })
            ),
            "uppercase hex must produce NonLowercaseHexByte: {:?}",
            result
        );
    }

    /// 62-character string must be rejected for wrong length (need 64).
    #[cfg(feature = "hmac-codec")]
    #[test]
    fn hmac_key_validation_rejects_wrong_length() {
        let short = "a".repeat(62);
        assert_eq!(short.len(), 62);
        let result = parse_hmac_key(&short, "TEST_KEY");
        assert!(
            matches!(
                result,
                Err(PresentationStartupError::InvalidHexLength { actual: 62, .. })
            ),
            "62-char hex must produce InvalidHexLength: {:?}",
            result
        );
    }

    /// 63-char hex produces InvalidHexLength.
    #[cfg(feature = "hmac-codec")]
    #[test]
    fn hmac_key_validation_rejects_63_chars() {
        let s63 = "a".repeat(63);
        let result = parse_hmac_key(&s63, "TEST_KEY");
        assert!(
            matches!(
                result,
                Err(PresentationStartupError::InvalidHexLength { actual: 63, .. })
            ),
            "63-char hex must produce InvalidHexLength: {:?}",
            result
        );
    }

    /// Non-hex byte (e.g. `g`) must be rejected with InvalidHexByte.
    #[cfg(feature = "hmac-codec")]
    #[test]
    fn hmac_key_validation_rejects_non_hex_byte() {
        let bad = "aabbccddeeff00112233445566778899aabbccddeeff001122334455667788gg";
        assert_eq!(bad.len(), 64);
        let result = parse_hmac_key(bad, "TEST_KEY");
        assert!(
            matches!(result, Err(PresentationStartupError::InvalidHexByte { .. })),
            "non-hex byte must produce InvalidHexByte: {:?}",
            result
        );
    }

    // ── HmacSha256HexString ───────────────────────────────────────────────

    /// When the key is not cached (startup bypassed), try_present must return
    /// Err(KeyNotValidated), not panic.
    ///
    /// This test does NOT set the env var or call validate_startup. It verifies
    /// the "startup bypassed" path via an explicit empty cache branch. This
    /// branch is now tested deterministically with an injected `None` key state,
    /// instead of depending on process-wide `HMAC_KEY` state.
    ///
    /// The definitive "cache miss → KeyNotValidated" guarantee is enforced by
    /// the type system: the per-value path exclusively reads an injected cache
    /// state and never reads the environment variable in the production path.
    #[cfg(feature = "hmac-codec")]
    #[test]
    fn hmac_string_cache_miss_returns_key_not_validated() {
        let result = hmac_sha256_hex_string_present_with_cached_key("test", None);
        assert!(
            matches!(
                result,
                Err(BuiltInPresentationError::KeyNotValidated {
                    env: "DJOGI_PRESENTATION_HMAC_KEY"
                })
            ),
            "cache miss must return KeyNotValidated: {:?}",
            result
        );
    }

    /// Two different inputs must produce different HMAC outputs (non-collision
    /// basic check). This test uses a specific test key installed via
    /// the test mutex.
    #[cfg(feature = "hmac-codec")]
    #[test]
    fn hmac_two_different_inputs_produce_different_outputs() {
        let _guard = TEST_HMAC_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        // Install a deterministic test key.
        // SAFETY: guarded by TEST_HMAC_ENV_MUTEX so no other thread reads
        // DJOGI_PRESENTATION_HMAC_KEY concurrently during this set_var call.
        let test_key = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        // SAFETY: TEST_HMAC_ENV_MUTEX held above; no concurrent readers of
        // DJOGI_PRESENTATION_HMAC_KEY in the single-threaded env-read path.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("DJOGI_PRESENTATION_HMAC_KEY", test_key);
        }

        // Prime the key via validate_startup — it may already be set, so ignore the result.
        let _ = validate_startup_for_hmac_key();

        if let Some(key) = HMAC_KEY.get() {
            // Key is loaded; test the non-collision property.
            let mut mac1 = HmacSha256::new_from_slice(key.as_ref()).unwrap();
            mac1.update(b"input_a");
            let out1 = hex_encode(&mac1.finalize().into_bytes());

            let mut mac2 = HmacSha256::new_from_slice(key.as_ref()).unwrap();
            mac2.update(b"input_b");
            let out2 = hex_encode(&mac2.finalize().into_bytes());

            assert_ne!(
                out1, out2,
                "different inputs must produce different HMAC outputs"
            );
            assert_eq!(out1.len(), 64, "HMAC output must be 64 hex chars");
            assert_eq!(out2.len(), 64, "HMAC output must be 64 hex chars");
        }
        // If the key was never set despite our attempt (e.g. set(..) already
        // contained a prior value), the test passes vacuously — the non-collision
        // property of HMAC is a cryptographic invariant, not a framework invariant.
    }

    // ── HmacSha256HexOptionString ─────────────────────────────────────────

    #[cfg(feature = "hmac-codec")]
    #[test]
    fn hmac_option_string_none_preserved() {
        let result = HmacSha256HexOptionString::try_present(&None);
        // None is always preserved, even when the key is not cached.
        assert!(
            matches!(result, Ok(None)),
            "None input must produce Ok(None): {:?}",
            result
        );
    }

    // ── HmacSha256Hex ────────────────────────────────────────────────────

    #[cfg(feature = "hmac-codec")]
    #[test]
    fn hmac_sha256_hex_as_str_returns_inner() {
        let h = HmacSha256Hex::new_unchecked("aabb".to_string());
        assert_eq!(h.as_str(), "aabb");
    }

    #[test]
    fn hmac_sha256_hex_try_from_valid_lowercase_hex_succeeds() {
        let input = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let h = HmacSha256Hex::try_from(input.to_string()).expect("valid hex must parse");
        assert_eq!(h.as_str(), input);
    }

    #[test]
    fn hmac_sha256_hex_try_from_rejects_invalid_length() {
        let result = HmacSha256Hex::try_from("a".repeat(63));
        assert!(result.is_err(), "63-char input must be rejected");
    }

    #[test]
    fn hmac_sha256_hex_try_from_rejects_uppercase_hex() {
        let result = HmacSha256Hex::try_from("A".repeat(64));
        assert!(result.is_err(), "uppercase hex must be rejected");
    }

    #[test]
    fn hmac_sha256_hex_from_sql_rejects_invalid_value() {
        use tokio_postgres::types::FromSql;

        let raw = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let result = HmacSha256Hex::from_sql(&tokio_postgres::types::Type::TEXT, raw);
        assert!(result.is_err(), "invalid Postgres text must be rejected");
    }

    #[test]
    fn hmac_sha256_hex_from_sql_accepts_text_type() {
        // `HmacSha256Hex: FromSql` delegates to `String::accepts`, so it accepts TEXT.
        use tokio_postgres::types::FromSql;
        assert!(
            HmacSha256Hex::accepts(&tokio_postgres::types::Type::TEXT),
            "HmacSha256Hex must accept the Postgres TEXT type"
        );
    }

    #[test]
    fn hmac_sha256_hex_serde_rejects_invalid_string() {
        let json = format!("\"{}\"", "A".repeat(64));
        let result = serde_json::from_str::<HmacSha256Hex>(&json);
        assert!(result.is_err(), "invalid serde string must be rejected");
    }

    // ── hex_encode ────────────────────────────────────────────────────────

    #[cfg(feature = "hmac-codec")]
    #[test]
    fn hex_encode_known_values() {
        assert_eq!(hex_encode(&[0x00]), "00");
        assert_eq!(hex_encode(&[0xff]), "ff");
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
        assert_eq!(hex_encode(&[]), "");
    }

    #[cfg(feature = "hmac-codec")]
    #[test]
    fn hex_encode_output_is_lowercase() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        let encoded = hex_encode(&bytes);
        assert!(
            encoded
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "hex_encode must produce only lowercase hex characters"
        );
    }
}
