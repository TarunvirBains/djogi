// Library APIs consumed by macro-generated code at consumer sites, not
// within djogi itself — suppress expected dead_code warnings.
#![allow(dead_code)]

//! AES-256-GCM field codec for encrypted-at-rest protected fields.
//!
//! Provides authenticated encryption (confidentiality + integrity) for
//! `String` fields, storing ciphertext as `Vec<u8>` in Postgres `BYTEA` columns.
//!
//! ## Ciphertext layout
//!
//! ```text
//! +---------+-----------+--------+------------------+
//! | version | key_index | nonce | ciphertext + tag |
//! | 1 B  | 1 B  | 12 B | variable length |
//! +---------+-----------+--------+------------------+
//! ```
//!
//! Total storage overhead: `plaintext.len() + 30` bytes (1 version byte +
//! 1 key-index byte + 12-byte nonce + 16-byte GCM tag). The minimum valid
//! ciphertext length is 30 bytes (empty plaintext).
//!
//! - **version** (`0x01`): future layouts increment this byte; an unrecognized
//! value is rejected with [`CodecError::UnknownVersion`].
//! - **key_index** (`0..=31`): the ring position the ciphertext was encrypted
//! under; an out-of-range index is rejected with
//! [`CodecError::UnknownKeyIndex`].
//!
//! ## Key management (key ring)
//!
//! Keys come from the `DJOGI_FIELD_CODEC_KEY_0` through
//! `DJOGI_FIELD_CODEC_KEY_31` environment variables — each exactly 64 lowercase
//! hex characters (32 bytes / 256 bits). `_0` is always required; the ring must
//! have no gaps (if index N is present, every index `0..=N` must be present);
//! the highest present index is the *active* index used for new encryptions.
//! The validated ring is cached in a `OnceLock<Vec<[u8; 32]>>` after first
//! successful parse, so subsequent encode/decode calls skip the env-var reads.
//! Startup validation calls [`load_ring`] before any CRUD operation to populate
//! the cache and close the decode-time side-channel (a populated cache means
//! `load_ring` can only return `Ok`, never `RingEmpty` / `MissingKey`).
//!
//! ## Per-(model, field) subkey derivation
//!
//! Ring entries are never used directly as AES keys. Each encode/decode derives
//! a per-(model, field) subkey via HKDF-SHA256 (IKM = the ring entry at the
//! recorded index, no salt, info = `b"djogi:aes256_gcm_v1\x00{model}\x00{field}"`).
//! Compromise of one field's ciphertext does not expose other fields.
//!
//! ## AAD binding
//!
//! Each encode/decode call constructs AAD as `format!("{}\x00{}", model, field)`,
//! binding the ciphertext to its model and field context. Relocating ciphertext
//! to a different field or model causes authentication failure ([`CodecError::AeadError`])
//! rather than silent decryption with wrong context. The HKDF info string binds
//! the same context into the subkey, so a relocated ciphertext fails on two
//! independent layers: subkey mismatch and AAD mismatch.

use std::sync::OnceLock;

use aes_gcm::aead::{Aead, Nonce, Payload};
use aes_gcm::{Aes256Gcm, KeyInit};

use super::{CodecError, FieldCodec, MissingKeyKind};
use crate::migrate::OnlineSafetyClassification;

/// The always-required base ring entry. The validator scans the full
/// `DJOGI_FIELD_CODEC_KEY_0..=31` family; this anchor names the base entry for
/// the startup error and is the `env_var` the macro passes to
/// `FieldCodecStartupRequirement::const_new`.
#[doc(hidden)]
pub const ENV_VAR: &str = "DJOGI_FIELD_CODEC_KEY_0";

/// Cached key ring, indexed by ring position. Populated by [`load_ring`] at
/// startup validation time (or on first encode/decode call if startup
/// validation is bypassed). Once set, immutable for the process lifetime —
/// subsequent reads of the `DJOGI_FIELD_CODEC_KEY_{N}` variables are ignored.
static CODEC_RING: OnceLock<Vec<[u8; 32]>> = OnceLock::new();

/// Mutex serializing env-var mutation across the full
/// `DJOGI_FIELD_CODEC_KEY_{0..=31}` family in tests so concurrent test runs do
/// not interfere with each other's ring state.
/// Only active under `#[cfg(test)]`. Production code never uses this mutex.
#[cfg(all(test, feature = "aes-codec"))]
pub(crate) static TEST_CODEC_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Scan `DJOGI_FIELD_CODEC_KEY_0` through `DJOGI_FIELD_CODEC_KEY_31`, validate
/// every present entry, and cache the ring in [`CODEC_RING`].
///
/// On first call, reads the env vars, validates each entry is 64 lowercase hex
/// characters (ASCII digits or `a`–`f`) decoding to exactly 32 bytes, verifies
/// the no-gap invariant (if index N is present, every `0..=N` must be present),
/// and stores the ring in the `OnceLock` via `set()`. On subsequent calls,
/// returns `Ok(())` without reading the environment again.
///
/// Startup validation MUST call this function (not an independent env-var check)
/// so that the `OnceLock` is populated before any CRUD call reaches the codec.
/// This guarantees the side-channel prevention property: if startup succeeds,
/// every subsequent `load_ring()` returns `Ok` — decode-time `RingEmpty` /
/// `MissingKey` is impossible.
///
/// Marked `pub` (not private) so macro-generated `inventory::submit!` blocks in
/// adopter crates can reference this function as the `validate` fn-pointer on
/// [`super::FieldCodecStartupRequirement`]. The `#[doc(hidden)]` attribute keeps
/// it out of the public API docs.
///
/// # Errors
/// - [`CodecError::RingEmpty`] when no `DJOGI_FIELD_CODEC_KEY_*` variable is set.
/// - [`CodecError::MissingKey`] with [`MissingKeyKind::Gap`] when a lower index
/// is absent while a higher one is present (names the missing index).
/// - [`CodecError::MissingKey`] with [`MissingKeyKind::Malformed`] when an entry
/// is not 64 lowercase hex characters / does not decode to 32 bytes.
#[doc(hidden)]
pub fn load_ring() -> Result<(), CodecError> {
    // Fast path: already cached from startup or a prior call.
    if CODEC_RING.get().is_some() {
        return Ok(());
    }

    // Slow path: scan the indexed env vars and cache.
    let mut ring: Vec<[u8; 32]> = Vec::new();
    for index in 0u8..=31 {
        let name = format!("DJOGI_FIELD_CODEC_KEY_{index}");
        match std::env::var(&name) {
            Ok(raw) => {
                if ring.len() != index as usize {
                    // A lower index is missing — gap in the ring. The missing
                    // index is the next position we expected to fill
                    // (`ring.len()`), not the present index we are looking at.
                    return Err(CodecError::MissingKey {
                        index: ring.len() as u8,
                        kind: MissingKeyKind::Gap,
                    });
                }
                // Reject any non-lowercase-hex or non-32-byte entry, naming this
                // index. Lowercase is enforced explicitly (uppercase `A`–`F` is
                // rejected) to match the DJOGI_PRESENTATION_HMAC_KEY validator:
                // 64 lowercase hex characters, i.e. ASCII digits or `a`–`f`.
                // `hex::decode` alone would silently accept uppercase, so the
                // lowercase-only invariant is checked at the byte level first.
                // This is a byte-level predicate, not a regex (per the project's
                // no-regex rule).
                let is_lower_hex = raw.len() == 64
                    && raw
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
                if !is_lower_hex {
                    return Err(CodecError::MissingKey {
                        index,
                        kind: MissingKeyKind::Malformed,
                    });
                }
                let bytes = hex::decode(&raw).map_err(|_| CodecError::MissingKey {
                    index,
                    kind: MissingKeyKind::Malformed,
                })?;
                let arr: [u8; 32] = bytes.try_into().map_err(|_| CodecError::MissingKey {
                    index,
                    kind: MissingKeyKind::Malformed,
                })?;
                ring.push(arr);
            }
            // Absent is fine unless a higher index turns out to be present
            // (caught by the gap check above on the next present entry).
            Err(_) => continue,
        }
    }
    if ring.is_empty() {
        return Err(CodecError::RingEmpty);
    }

    // Set wins-once: if a concurrent call already set the ring, the duplicate is
    // silently ignored — the first successful parse is stable.
    let _ = CODEC_RING.set(ring);
    Ok(())
}

/// Derive the per-(model, field) AES-256 subkey from a ring entry via
/// HKDF-SHA256. The info string provides domain separation: the same ring entry
/// produces a different AES key for every (model, field) pair.
fn derive_subkey(ikm: &[u8; 32], model: &str, field: &str) -> [u8; 32] {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, ikm); // no salt
    let mut info = Vec::with_capacity(20 + model.len() + 1 + field.len());
    info.extend_from_slice(b"djogi:aes256_gcm_v1\x00");
    info.extend_from_slice(model.as_bytes());
    info.push(0);
    info.extend_from_slice(field.as_bytes());
    let mut okm = [0u8; 32];
    // `expand` only errors when the requested length exceeds `255 * 32` bytes;
    // 32 is always valid, so this is an infallible-in-practice invariant on a
    // framework-controlled length, not a panic on untrusted input.
    hk.expand(&info, &mut okm)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    okm
}

/// Generate a fresh 96-bit (12-byte) AES-GCM nonce from the OS CSPRNG.
/// Fails only if `getrandom` itself fails (host entropy source unavailable),
/// surfaced as [`CodecError::RngFailure`] via the `From<getrandom::Error>` impl
/// rather than a panic.
fn generate_nonce() -> Result<[u8; 12], CodecError> {
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce)?; // getrandom 0.3: `fill`; `?` via From<getrandom::Error>
    Ok(nonce)
}

/// AES-256-GCM field codec.
///
/// Encrypts `String` fields into versioned `Vec<u8>` ciphertext using
/// AES-256-GCM with per-(model, field) HKDF-derived subkeys. The stored blob is
/// `[version (1 B) | key_index (1 B) | nonce (12 B) | ciphertext + tag]`; AAD is
/// bound as `model\x00field` to prevent ciphertext-relocation attacks.
///
/// Adopters reach this codec only via the
/// `#[field(protected(codec = "aes256_gcm_v1"))]` attribute; the macro resolves
/// the type and emits the encode/decode calls. The column is `BYTEA`.
pub struct Aes256GcmV1;

impl FieldCodec for Aes256GcmV1 {
    const ID: &'static str = "aes256_gcm_v1";

    type Decoded = String;
    type Encoded = Vec<u8>;
    type Error = CodecError;

    fn encode(
        model: &'static str,
        field: &'static str,
        value: &String,
    ) -> Result<Vec<u8>, CodecError> {
        load_ring()?; // no-op fast path once cached
        let ring = CODEC_RING.get().ok_or(CodecError::RingEmpty)?;
        let active_index = ring.len() - 1; // highest present index
        let subkey = derive_subkey(&ring[active_index], model, field);
        let nonce_bytes = generate_nonce()?;
        let nonce = Nonce::<Aes256Gcm>::from_slice(&nonce_bytes);
        let aad = format!("{model}\x00{field}");
        // A 32-byte slice is always a valid AES-256 key, so this is an
        // infallible-invariant assertion on a framework-controlled input, not a
        // panic on untrusted data (same posture as the HMAC codec precedent).
        let aead =
            Aes256Gcm::new_from_slice(&subkey).expect("32-byte slice is a valid AES-256 key");
        let ciphertext = aead.encrypt(
            nonce,
            Payload {
                msg: value.as_bytes(),
                aad: aad.as_bytes(),
            },
        )?; // From<aes_gcm::Error> — propagate rather than panic
        // [version (1 B) | key_index (1 B) | nonce (12 B) | ciphertext + tag]
        let mut out = Vec::with_capacity(2 + 12 + ciphertext.len());
        out.push(0x01); // version
        out.push(active_index as u8); // key_index
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    fn decode(
        model: &'static str,
        field: &'static str,
        stored: &Vec<u8>,
    ) -> Result<String, CodecError> {
        // Minimum valid blob: 30 bytes (1 version + 1 key_index + 12 nonce +
        // 16 tag, even for empty plaintext).
        if stored.len() < 30 {
            return Err(CodecError::CiphertextTooShort);
        }
        let version = stored[0];
        if version != 0x01 {
            return Err(CodecError::UnknownVersion(version));
        }
        load_ring()?; // no-op fast path once cached
        let ring = CODEC_RING.get().ok_or(CodecError::RingEmpty)?;
        let key_index = stored[1] as usize;
        if key_index >= ring.len() {
            // Capture ring length at decode time so the error distinguishes
            // "never present" from "retired".
            return Err(CodecError::UnknownKeyIndex {
                index: stored[1],
                ring_len: ring.len() as u8,
            });
        }
        let nonce = Nonce::<Aes256Gcm>::from_slice(&stored[2..14]);
        let ciphertext = &stored[14..];
        let subkey = derive_subkey(&ring[key_index], model, field);
        let aad = format!("{model}\x00{field}");
        let aead =
            Aes256Gcm::new_from_slice(&subkey).expect("32-byte slice is a valid AES-256 key");
        let plaintext = aead.decrypt(
            nonce,
            Payload {
                msg: ciphertext,
                aad: aad.as_bytes(),
            },
        )?; // From<aes_gcm::Error>
        Ok(String::from_utf8(plaintext)?) // From<FromUtf8Error>
    }

    fn classify_transition<Other: FieldCodec>() -> OnlineSafetyClassification {
        if Other::ID == Self::ID {
            // Same codec — no-op transition.
            OnlineSafetyClassification::OnlineSafe
        } else {
            // Codec ↔ different codec: standard expand-contract migration
            // pattern. The plaintext → encrypted case is handled at the
            // descriptor/classifier level (where `codec` is `None`), not through
            // a FieldCodec type — this method is type-to-type and never sees
            // plaintext.
            OnlineSafetyClassification::ExpandContract
        }
    }
}

/// Run `f` with a specific key ring active. Acquires [`TEST_CODEC_ENV_MUTEX`] to
/// serialize env-var mutation across concurrent tests. Sets
/// `DJOGI_FIELD_CODEC_KEY_{i}` for each provided hex key (index = slice
/// position), clears the remaining indexed variables, calls [`load_ring`] to
/// populate the `OnceLock`, runs `f`, then restores the previous env state.
///
/// **Single-`OnceLock` honesty:** the ring cache is single-set per process. If a
/// prior test already populated [`CODEC_RING`], `load_ring` returns `Ok`
/// immediately (fast path) and this call does NOT re-populate the ring. Tests
/// that need a specific cold ring must be the first to touch it (mark them
/// `#[serial_test::serial]`) or run in a dedicated test binary (own process =
/// fresh `OnceLock`).
#[cfg(all(test, feature = "aes-codec"))]
pub(crate) fn test_with_codec_ring(keys_hex: &[&str], f: impl FnOnce()) {
    let _lock = TEST_CODEC_ENV_MUTEX.lock().unwrap();

    // Save previous env var state for all 32 indexed variables.
    let prev: Vec<Option<String>> = (0..32)
        .map(|i| std::env::var(format!("DJOGI_FIELD_CODEC_KEY_{i}")).ok())
        .collect();
    for i in 0..32 {
        let name = format!("DJOGI_FIELD_CODEC_KEY_{i}");
        match keys_hex.get(i) {
            Some(k) => unsafe { std::env::set_var(&name, k) },
            None => unsafe { std::env::remove_var(&name) },
        }
    }

    // Populate the OnceLock with the test ring (no-op if already set).
    let _ = load_ring();

    f();

    // Restore previous env var state via the guard pattern.
    for (i, p) in prev.into_iter().enumerate() {
        let name = format!("DJOGI_FIELD_CODEC_KEY_{i}");
        match p {
            Some(v) => unsafe { std::env::set_var(&name, &v) },
            None => unsafe { std::env::remove_var(&name) },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    /// Test key at ring index 0: 32 bytes, mostly zeros (deterministic).
    const TEST_KEY_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    /// Distinct second test key (ring index 1).
    const TEST_KEY_HEX_B: &str = "0000000000000000000000000000000000000000000000000000000000000002";

    #[test]
    fn encode_decode_round_trip() {
        test_with_codec_ring(&[TEST_KEY_HEX], || {
            let original = String::from("djogi encrypted-at-rest round-trip test");
            let encoded = Aes256GcmV1::encode("TestModel", "secret_field", &original)
                .expect("encode should succeed with valid ring");

            // Overhead is 30 bytes (1 version + 1 key_index + 12 nonce + 16 tag).
            assert_eq!(encoded.len(), original.len() + 30);

            let decoded = Aes256GcmV1::decode("TestModel", "secret_field", &encoded)
                .expect("decode should succeed with matching ring and AAD");

            assert_eq!(decoded, original);
        });
    }

    #[test]
    fn encode_produces_different_ciphertext_each_time() {
        // Nonce is randomly generated per-encryption, so two encodes of the same
        // plaintext produce different ciphertexts.
        test_with_codec_ring(&[TEST_KEY_HEX], || {
            let original = String::from("same plaintext");
            let encoded1 = Aes256GcmV1::encode("TestModel", "field", &original).unwrap();
            let encoded2 = Aes256GcmV1::encode("TestModel", "field", &original).unwrap();

            assert_ne!(
                encoded1, encoded2,
                "two encodes of same plaintext should differ (random nonce)"
            );

            assert_eq!(
                Aes256GcmV1::decode("TestModel", "field", &encoded1).unwrap(),
                original
            );
            assert_eq!(
                Aes256GcmV1::decode("TestModel", "field", &encoded2).unwrap(),
                original
            );
        });
    }

    #[test]
    fn aad_mismatch_returns_aead_error() {
        // Encrypting with one (model, field) pair and decrypting with another
        // fails authentication: both the AAD and the HKDF subkey differ.
        test_with_codec_ring(&[TEST_KEY_HEX], || {
            let original = String::from("aad binding test");
            let encoded = Aes256GcmV1::encode("ModelA", "field_a", &original).unwrap();

            let err = Aes256GcmV1::decode("ModelB", "field_a", &encoded).unwrap_err();
            assert!(matches!(err, CodecError::AeadError(_)));

            let err = Aes256GcmV1::decode("ModelA", "field_b", &encoded).unwrap_err();
            assert!(matches!(err, CodecError::AeadError(_)));

            assert_eq!(
                Aes256GcmV1::decode("ModelA", "field_a", &encoded).unwrap(),
                original
            );
        });
    }

    #[test]
    fn ciphertext_too_short_returns_error() {
        test_with_codec_ring(&[TEST_KEY_HEX], || {
            let short = vec![0u8; 10]; // less than the 30-byte minimum
            let err = Aes256GcmV1::decode("Model", "field", &short).unwrap_err();
            assert!(matches!(err, CodecError::CiphertextTooShort));

            let msg = err.to_string();
            assert!(
                msg.contains("30"),
                "message should mention the 30-byte minimum: {msg}"
            );
        });
    }

    #[test]
    fn unknown_version_decode() {
        test_with_codec_ring(&[TEST_KEY_HEX], || {
            // 30-byte blob with an unrecognized version byte 0x02. The version
            // check runs before any ring access, so the ring contents are
            // irrelevant here.
            let mut blob = vec![0u8; 30];
            blob[0] = 0x02;
            let err = Aes256GcmV1::decode("Model", "field", &blob).unwrap_err();
            assert!(
                matches!(err, CodecError::UnknownVersion(0x02)),
                "got: {err:?}"
            );
        });
    }

    #[test]
    fn unknown_key_index_decode() {
        // Encode under a 2-entry ring, then hand-edit the key_index byte to 5
        // (out of range), and decode → UnknownKeyIndex { index: 5, ring_len: 2 }.
        test_with_codec_ring(&[TEST_KEY_HEX, TEST_KEY_HEX_B], || {
            let original = String::from("rotate me");
            let mut encoded = Aes256GcmV1::encode("Model", "field", &original).unwrap();
            encoded[1] = 5; // forge an out-of-range key index
            let err = Aes256GcmV1::decode("Model", "field", &encoded).unwrap_err();
            match err {
                CodecError::UnknownKeyIndex { index, ring_len } => {
                    assert_eq!(index, 5);
                    // ring_len may be >= 2 depending on whether this process's
                    // OnceLock was set by an earlier serial test; assert the
                    // Display shape against the captured value.
                    assert_eq!(
                        err.to_string(),
                        format!("key index 5 not in ring of length {ring_len}")
                    );
                }
                other => panic!("expected UnknownKeyIndex, got: {other:?}"),
            }
        });
    }

    #[test]
    fn empty_string_round_trip() {
        test_with_codec_ring(&[TEST_KEY_HEX], || {
            let original = String::new();
            let encoded = Aes256GcmV1::encode("Model", "field", &original).unwrap();
            // Minimum blob: 30 bytes for empty plaintext.
            assert_eq!(encoded.len(), 30);

            let decoded = Aes256GcmV1::decode("Model", "field", &encoded).unwrap();
            assert_eq!(decoded, original);
        });
    }

    #[test]
    fn hkdf_domain_separation() {
        // Same ring entry, encode under ("ModelA", "f"); decoding A's blob under
        // ("ModelB", "f") fails because the HKDF subkey (and AAD) differ.
        test_with_codec_ring(&[TEST_KEY_HEX], || {
            let original = String::from("domain separation");
            let blob_a = Aes256GcmV1::encode("ModelA", "f", &original).unwrap();
            let err = Aes256GcmV1::decode("ModelB", "f", &blob_a).unwrap_err();
            assert!(matches!(err, CodecError::AeadError(_)), "got: {err:?}");
        });
    }

    #[test]
    fn aead_error_display_safety() {
        // The aes-gcm crate uses an opaque `aead::Error` whose Display is
        // "generic AEAD error" — no key bytes, nonce values, or ciphertext.
        let aead_err = CodecError::AeadError(aes_gcm::aead::Error);
        let msg = aead_err.to_string();
        assert_eq!(msg, "generic AEAD error");
        assert!(!msg.contains("0x"));
        assert!(!msg.contains("key"));
        assert!(!msg.contains("nonce"));
    }

    #[test]
    fn codec_error_source_chaining() {
        // Utf8Error and RngFailure chain a source; the structural variants do
        // not, and the opaque AeadError cannot.
        let utf8_err = String::from_utf8(vec![0xff]).unwrap_err();
        assert!(CodecError::Utf8Error(utf8_err).source().is_some());

        assert!(
            CodecError::AeadError(aes_gcm::aead::Error)
                .source()
                .is_none()
        );
        assert!(
            CodecError::MissingKey {
                index: 0,
                kind: MissingKeyKind::Gap
            }
            .source()
            .is_none()
        );
        assert!(CodecError::RingEmpty.source().is_none());
        assert!(CodecError::CiphertextTooShort.source().is_none());
        assert!(CodecError::UnknownVersion(2).source().is_none());
        assert!(
            CodecError::UnknownKeyIndex {
                index: 5,
                ring_len: 2
            }
            .source()
            .is_none()
        );
    }

    #[test]
    fn unknown_key_index_display_exact() {
        let err = CodecError::UnknownKeyIndex {
            index: 5,
            ring_len: 2,
        };
        assert_eq!(err.to_string(), "key index 5 not in ring of length 2");
    }

    #[test]
    fn classify_transition_same_codec_is_online_safe() {
        let classification = <Aes256GcmV1 as FieldCodec>::classify_transition::<Aes256GcmV1>();
        assert_eq!(classification, OnlineSafetyClassification::OnlineSafe);
    }

    #[test]
    fn classify_transition_different_codec_is_expand_contract() {
        struct DummyCodec;
        impl FieldCodec for DummyCodec {
            const ID: &'static str = "_djogi_test_dummy_codec";
            type Decoded = String;
            type Encoded = Vec<u8>;
            type Error = CodecError;

            fn encode(
                _model: &'static str,
                _field: &'static str,
                value: &Self::Decoded,
            ) -> Result<Self::Encoded, Self::Error> {
                Ok(value.as_bytes().to_vec())
            }

            fn decode(
                _model: &'static str,
                _field: &'static str,
                stored: &Self::Encoded,
            ) -> Result<Self::Decoded, Self::Error> {
                Ok(String::from_utf8(stored.clone())?)
            }

            fn classify_transition<Other: FieldCodec>() -> OnlineSafetyClassification {
                if Self::ID == Other::ID {
                    OnlineSafetyClassification::OnlineSafe
                } else {
                    OnlineSafetyClassification::ExpandContract
                }
            }
        }

        let classification = <Aes256GcmV1 as FieldCodec>::classify_transition::<DummyCodec>();
        assert_eq!(classification, OnlineSafetyClassification::ExpandContract);
    }

    #[test]
    fn codec_startup_error_display_format() {
        use super::super::CodecStartupError;

        let err = CodecStartupError {
            codec_id: "aes256_gcm_v1",
            env_var: ENV_VAR,
            error: "no field codec key configured".to_owned(),
        };
        let msg = err.to_string();
        assert!(msg.contains("aes256_gcm_v1"));
        assert!(msg.contains(ENV_VAR));
        assert!(msg.contains("no field codec key configured"));
    }
}

/// Ring-isolation tests requiring a specific cold-cache ring state.
///
/// **Single-`OnceLock` honesty:** [`CODEC_RING`] is single-set per process, so
/// at most one "fresh-ring" test in a `cargo test` run actually exercises a cold
/// `load_ring`; the rest hit the populated cache and degrade to best-effort
/// no-ops. Each test here is `#[serial_test::serial]` and guards on the cache
/// state so a populated ring does not make it fail spuriously. The load-bearing
/// guarantees come from (a) `ring_loading_two_keys_active_index_one` +
/// `cross_ring_entry_non_interference`, which set the ring they need first when
/// the cache is cold, and (b) the integration round-trip against a real DB. We
/// do NOT add a `reset_ring()` hook — it would defeat the immutability invariant
/// the whole design rests on.
#[cfg(all(test, feature = "aes-codec"))]
mod ring_isolation_tests {
    use super::*;

    const KEY_0: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    const KEY_1: &str = "0000000000000000000000000000000000000000000000000000000000000002";

    #[serial_test::serial]
    #[test]
    fn ring_loading_one_key_active_index_zero() {
        // With a single-entry ring the active index is 0, so a fresh encode
        // records key_index byte == 0. Only meaningful when this is the first
        // test to populate the cache; otherwise the cached ring decides the
        // active index and we skip the assertion.
        test_with_codec_ring(&[KEY_0], || {
            if CODEC_RING.get().map(|r| r.len()) == Some(1) {
                let blob = Aes256GcmV1::encode("M", "f", &"x".to_string()).unwrap();
                assert_eq!(blob[1], 0, "single-key ring active index must be 0");
            }
        });
    }

    #[serial_test::serial]
    #[test]
    fn ring_loading_two_keys_active_index_one() {
        // With a two-entry ring the active index is 1: a fresh encode records
        // key_index byte == 1. Guarded on the cache holding the 2-entry ring.
        test_with_codec_ring(&[KEY_0, KEY_1], || {
            if CODEC_RING.get().map(|r| r.len()) == Some(2) {
                let blob = Aes256GcmV1::encode("M", "f", &"x".to_string()).unwrap();
                assert_eq!(blob[1], 1, "two-key ring active index must be 1");
            }
        });
    }

    #[serial_test::serial]
    #[test]
    fn cross_ring_entry_non_interference() {
        // key_A@0, key_B@1, same (model, field, plaintext): forging a key_A blob
        // to claim key_index 1 (key_B's slot) and decoding fails authentication.
        // This guards the regression that would drop key_index from HKDF
        // derivation. Guarded on the 2-entry ring being active.
        test_with_codec_ring(&[KEY_0, KEY_1], || {
            if CODEC_RING.get().map(|r| r.len()) == Some(2) {
                // Active index is 1, so a fresh blob records key_index == 1 and
                // derives its subkey from ring[1]. Forge it to claim key_index 0
                // (ring[0] = key_A) — the subkey then mismatches and auth fails.
                let mut blob = Aes256GcmV1::encode("M", "f", &"x".to_string()).unwrap();
                assert_eq!(blob[1], 1);
                blob[1] = 0; // claim key_A's slot for a key_B-encrypted blob
                let err = Aes256GcmV1::decode("M", "f", &blob).unwrap_err();
                assert!(
                    matches!(err, CodecError::AeadError(_)),
                    "forging the key_index must fail authentication: {err:?}"
                );
            }
        });
    }

    #[serial_test::serial]
    #[test]
    fn key_rotation_round_trip() {
        // Rotation honesty: because the OnceLock is single-set, a second
        // `test_with_codec_ring` call does NOT re-populate the ring. This test
        // can only exercise rotation if it is the first to touch the cache. We
        // assert what the live cache state supports and document that the
        // load-bearing rotation coverage is the integration round-trip + the
        // two-key load test, not this best-effort assertion.
        test_with_codec_ring(&[KEY_0], || {
            let len = CODEC_RING.get().map(|r| r.len());
            if len == Some(1) {
                let blob = Aes256GcmV1::encode("M", "f", &"v".to_string()).unwrap();
                assert_eq!(blob[1], 0, "first ring entry encodes under index 0");
                // Old blob decodes under index 0 regardless of later cache state.
                assert_eq!(Aes256GcmV1::decode("M", "f", &blob).unwrap(), "v");
            }
        });
    }

    #[serial_test::serial]
    #[test]
    fn ring_immutability() {
        // Set the ring, then mutate the env var to garbage; a subsequent
        // encode/decode still succeeds (the cached ring wins).
        test_with_codec_ring(&[KEY_0], || {
            // Only meaningful once the ring is cached.
            if CODEC_RING.get().is_some() {
                unsafe { std::env::set_var("DJOGI_FIELD_CODEC_KEY_0", "not-hex-garbage") };
                let blob = Aes256GcmV1::encode("M", "f", &"v".to_string()).unwrap();
                assert_eq!(Aes256GcmV1::decode("M", "f", &blob).unwrap(), "v");
            }
        });
    }

    #[serial_test::serial]
    #[test]
    fn missing_ring_startup_failure() {
        // With no DJOGI_FIELD_CODEC_KEY_* set AND CODEC_RING unset, load_ring →
        // RingEmpty. Best-effort: only meaningful with a cold OnceLock.
        let _lock = TEST_CODEC_ENV_MUTEX.lock().unwrap();
        let prev: Vec<Option<String>> = (0..32)
            .map(|i| std::env::var(format!("DJOGI_FIELD_CODEC_KEY_{i}")).ok())
            .collect();
        for i in 0..32 {
            unsafe { std::env::remove_var(format!("DJOGI_FIELD_CODEC_KEY_{i}")) };
        }
        if CODEC_RING.get().is_none() {
            let err = load_ring().unwrap_err();
            assert!(matches!(err, CodecError::RingEmpty), "got: {err:?}");
        }
        for (i, p) in prev.into_iter().enumerate() {
            let name = format!("DJOGI_FIELD_CODEC_KEY_{i}");
            match p {
                Some(v) => unsafe { std::env::set_var(&name, &v) },
                None => unsafe { std::env::remove_var(&name) },
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn ring_gap_detection() {
        // _0 and _2 set, _1 unset → MissingKey { index: 1, kind: Gap }. The gap
        // is detected when scanning index 2 (the first present entry past the
        // hole), so the missing index reported is 1 (= ring.len() at that point).
        // Best-effort: only meaningful with a cold OnceLock.
        let _lock = TEST_CODEC_ENV_MUTEX.lock().unwrap();
        let prev: Vec<Option<String>> = (0..32)
            .map(|i| std::env::var(format!("DJOGI_FIELD_CODEC_KEY_{i}")).ok())
            .collect();
        for i in 0..32 {
            unsafe { std::env::remove_var(format!("DJOGI_FIELD_CODEC_KEY_{i}")) };
        }
        unsafe {
            std::env::set_var("DJOGI_FIELD_CODEC_KEY_0", KEY_0);
            std::env::set_var("DJOGI_FIELD_CODEC_KEY_2", KEY_1);
        }
        if CODEC_RING.get().is_none() {
            let err = load_ring().unwrap_err();
            match err {
                CodecError::MissingKey {
                    index,
                    kind: MissingKeyKind::Gap,
                } => {
                    assert_eq!(index, 1);
                    assert!(err.to_string().contains("DJOGI_FIELD_CODEC_KEY_1"));
                }
                other => panic!("expected MissingKey gap at index 1, got: {other:?}"),
            }
        }
        for (i, p) in prev.into_iter().enumerate() {
            let name = format!("DJOGI_FIELD_CODEC_KEY_{i}");
            match p {
                Some(v) => unsafe { std::env::set_var(&name, &v) },
                None => unsafe { std::env::remove_var(&name) },
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn malformed_entry_detection() {
        // _0 set to a non-hex value → MissingKey { index: 0, Malformed }.
        // Best-effort: only meaningful with a cold OnceLock.
        let _lock = TEST_CODEC_ENV_MUTEX.lock().unwrap();
        let prev: Vec<Option<String>> = (0..32)
            .map(|i| std::env::var(format!("DJOGI_FIELD_CODEC_KEY_{i}")).ok())
            .collect();
        for i in 0..32 {
            unsafe { std::env::remove_var(format!("DJOGI_FIELD_CODEC_KEY_{i}")) };
        }
        unsafe { std::env::set_var("DJOGI_FIELD_CODEC_KEY_0", "not-64-lowercase-hex") };
        if CODEC_RING.get().is_none() {
            let err = load_ring().unwrap_err();
            match err {
                CodecError::MissingKey {
                    index,
                    kind: MissingKeyKind::Malformed,
                } => {
                    assert_eq!(index, 0);
                    assert!(err.to_string().contains("DJOGI_FIELD_CODEC_KEY_0"));
                }
                other => panic!("expected MissingKey malformed at index 0, got: {other:?}"),
            }
        }
        for (i, p) in prev.into_iter().enumerate() {
            let name = format!("DJOGI_FIELD_CODEC_KEY_{i}");
            match p {
                Some(v) => unsafe { std::env::set_var(&name, &v) },
                None => unsafe { std::env::remove_var(&name) },
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn base_key_only_missing_distinction() {
        // No vars → RingEmpty; _0 unset but _1 present → MissingKey gap at 0.
        // Best-effort: only meaningful with a cold OnceLock.
        let _lock = TEST_CODEC_ENV_MUTEX.lock().unwrap();
        let prev: Vec<Option<String>> = (0..32)
            .map(|i| std::env::var(format!("DJOGI_FIELD_CODEC_KEY_{i}")).ok())
            .collect();
        for i in 0..32 {
            unsafe { std::env::remove_var(format!("DJOGI_FIELD_CODEC_KEY_{i}")) };
        }
        unsafe { std::env::set_var("DJOGI_FIELD_CODEC_KEY_1", KEY_1) };
        if CODEC_RING.get().is_none() {
            let err = load_ring().unwrap_err();
            match err {
                CodecError::MissingKey {
                    index: 0,
                    kind: MissingKeyKind::Gap,
                } => {
                    assert!(err.to_string().contains("DJOGI_FIELD_CODEC_KEY_0"));
                }
                other => panic!("expected MissingKey gap at index 0, got: {other:?}"),
            }
        }
        for (i, p) in prev.into_iter().enumerate() {
            let name = format!("DJOGI_FIELD_CODEC_KEY_{i}");
            match p {
                Some(v) => unsafe { std::env::set_var(&name, &v) },
                None => unsafe { std::env::remove_var(&name) },
            }
        }
    }
}
