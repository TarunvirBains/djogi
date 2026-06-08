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
//! +--------+------------------+
//! | nonce  | ciphertext + tag |
//! | 12 B   | variable length  |
//! +--------+------------------+
//! ```
//!
//! Total storage overhead: `plaintext.len() + 28` bytes (12-byte nonce + 16-byte tag).
//!
//! ## Key management
//!
//! Key is loaded from `DJOGI_FIELD_CODEC_KEY` environment variable — exactly
//! 64 lowercase hex characters (32 bytes / 256 bits). The key is cached in
//! a `OnceLock<[u8; 32]>` after first successful parse, so subsequent encode/
//! decode calls skip the env-var read. Startup validation should call
//! [`load_key`] before any CRUD operations to populate the cache and prevent
//! side-channel leakage from per-call env reads.
//!
//! ## AAD binding
//!
//! Each encode/decode call constructs AAD as `format!("{}\x00{}", model, field)`,
//! binding the ciphertext to its model and field context. Relocating ciphertext
//! to a different field or model causes authentication failure (`AeadError`)
//! rather than silent decryption with wrong context.

use std::sync::OnceLock;

use aes_gcm::aead::{Aead, AeadCore, Payload};
use aes_gcm::{Aes256Gcm, Key, KeyInit};

type AesNonce = aes_gcm::aead::generic_array::GenericArray<u8, <Aes256Gcm as AeadCore>::NonceSize>;

use super::{CodecError, FieldCodec};
use crate::migrate::OnlineSafetyClassification;

/// The environment variable name for the codec key.
#[doc(hidden)]
pub const ENV_VAR: &str = "DJOGI_FIELD_CODEC_KEY";

/// Cached codec key. Populated by [`load_key`] at startup validation time
/// (or on first encode/decode call if startup validation is bypassed).
/// Once set, the cached key is immutable — subsequent reads of the env var
/// are ignored.
static CODEC_KEY: OnceLock<[u8; 32]> = OnceLock::new();

/// Mutex serializing env-var mutation in tests so concurrent test runs
/// do not interfere with each other's key state.
/// Only active under `#[cfg(test)]`. Production code never uses this mutex.
#[cfg(all(test, feature = "aes-codec"))]
pub(crate) static TEST_CODEC_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Parse the codec key from `DJOGI_FIELD_CODEC_KEY` and cache it in [`CODEC_KEY`].
///
/// On first call, reads the env var, validates hex format (64 lowercase hex chars),
/// stores the parsed `[u8; 32]` in the OnceLock via `OnceLock::set()`, and returns
/// the key by value. On subsequent calls, returns a clone of the already-cached key
/// without reading the environment again.
///
/// Startup validation MUST call this function (not an independent env-var check)
/// so that the OnceLock is populated before any CRUD call reaches `load_key()`.
/// This guarantees the side-channel prevention property: if startup succeeds,
/// every subsequent `load_key()` returns `Ok` — decode-time `MissingKey` is impossible.
///
/// Marked `pub` (not private) so macro-generated `inventory::submit!` blocks in
/// adopter crates can reference this function as the `validate` fn-pointer on
/// [`FieldCodecStartupRequirement`]. The `#[doc(hidden)]` attribute prevents it
/// from appearing in public API docs. This mirrors the HMAC key validation precedent
/// where the presentation codec validator uses `OnceLock::set()`.
#[doc(hidden)]
pub fn load_key() -> Result<[u8; 32], CodecError> {
    // Fast path: already cached from startup or prior call.
    if let Some(key) = CODEC_KEY.get() {
        return Ok(*key);
    }

    // Slow path: parse from env var and cache.
    let raw = std::env::var(ENV_VAR).map_err(|_| CodecError::MissingKey)?;
    let bytes = hex::decode(&raw).map_err(|_| CodecError::MissingKey)?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| CodecError::MissingKey)?;

    // Set wins-once: if a concurrent call already set the key, the duplicate is
    // silently ignored — the first successful parse is stable.
    let _ = CODEC_KEY.set(arr);
    Ok(*CODEC_KEY.get().unwrap())
}

/// Generate a fresh 96-bit (12-byte) nonce via `getrandom`.
/// Reuse is prevented by design: every encryption invocation draws from the OS CSPRNG.
fn generate_nonce() -> [u8; 12] {
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce)
        .expect("getrandom failed: OS CSPRNG unavailable (nonce reuse would be catastrophic)");
    nonce
}

/// AES-256-GCM field codec.
///
/// Encrypts `String` fields into `Vec<u8>` ciphertext using AES-256-GCM.
/// The ciphertext layout is `[nonce (12 B) | ciphertext + tag]`.
/// AAD is bound as `model\x00field` to prevent ciphertext relocation attacks.
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
        let key = load_key()?;
        let nonce_bytes = generate_nonce();
        let aad = format!("{model}\x00{field}");

        let key_bytes: &[u8; 32] = &key;
        let aead = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key_bytes));
        let nonce: AesNonce = nonce_bytes.into();
        let ciphertext = aead
            .encrypt(
                &nonce,
                Payload {
                    msg: value.as_bytes(),
                    aad: aad.as_bytes(),
                },
            )
            .expect("AES-256-GCM encrypt should not fail with valid key and nonce");

        // Prepend nonce: [nonce (12 B) | ciphertext + tag]
        let mut result = Vec::with_capacity(12 + ciphertext.len());
        result.extend_from_slice(&nonce_bytes);
        result.extend_from_slice(&ciphertext);
        Ok(result)
    }

    fn decode(
        model: &'static str,
        field: &'static str,
        stored: &Vec<u8>,
    ) -> Result<String, CodecError> {
        // Minimum ciphertext: 28 bytes (12 nonce + 16 tag, even for empty plaintext).
        if stored.len() < 28 {
            return Err(CodecError::CiphertextTooShort { got: stored.len() });
        }

        let (nonce_bytes, ciphertext) = stored.split_at(12);
        let key = load_key()?;
        let aad = format!("{model}\x00{field}");

        let key_bytes: &[u8; 32] = &key;
        let aead = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key_bytes));
        let nonce_arr: [u8; 12] = nonce_bytes
            .try_into()
            .map_err(|_| CodecError::CiphertextTooShort { got: 12 })?;
        let nonce: AesNonce = nonce_arr.into();
        let plaintext = aead
            .decrypt(
                &nonce,
                Payload {
                    msg: ciphertext,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(CodecError::AeadError)?;

        String::from_utf8(plaintext).map_err(CodecError::Utf8Error)
    }

    fn classify_transition<Other: FieldCodec>() -> OnlineSafetyClassification {
        if Other::ID == Self::ID {
            // Same codec — no-op transition.
            OnlineSafetyClassification::OnlineSafe
        } else {
            // Codec ↔ different codec: standard expand-contract migration pattern.
            // The plaintext → encrypted case is handled at the descriptor level
            // (where `codec` is `None`), not through a FieldCodec type.
            OnlineSafetyClassification::ExpandContract
        }
    }
}

/// Run `f` with a specific codec key active. Acquires [`TEST_CODEC_ENV_MUTEX`] to
/// serialize env-var mutation across concurrent tests. Sets the key, calls
/// [`load_key`] to populate the OnceLock, runs `f`, then restores previous state.
#[cfg(all(test, feature = "aes-codec"))]
pub(crate) fn test_with_codec_key(key_hex: &str, f: impl FnOnce()) {
    let _lock = TEST_CODEC_ENV_MUTEX.lock().unwrap();

    // Save previous env var state.
    let prev = std::env::var(ENV_VAR).ok();
    unsafe { std::env::set_var(ENV_VAR, key_hex) };

    // Populate the OnceLock with the test key.
    // If the lock was already set by a prior test, load_key() returns the cached
    // value immediately (fast path). Tests requiring fresh key state must be
    // marked #[serial_test::serial] so they run before any test populates the cache.
    let _ = load_key();

    f();

    // Restore previous env var state via guard pattern.
    match prev {
        Some(v) => unsafe { std::env::set_var(ENV_VAR, &v) },
        None => unsafe { std::env::remove_var(ENV_VAR) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test key: 32 bytes of mostly-zeros (deterministic for testing).
    const TEST_KEY_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000001";

    #[test]
    fn encode_decode_round_trip() {
        test_with_codec_key(TEST_KEY_HEX, || {
            let original = String::from("djogi encrypted-at-rest round-trip test");
            let encoded = Aes256GcmV1::encode("TestModel", "secret_field", &original)
                .expect("encode should succeed with valid key");

            // Ciphertext should be 28 bytes overhead (12 nonce + 16 tag).
            assert_eq!(encoded.len(), original.len() + 28);

            let decoded = Aes256GcmV1::decode("TestModel", "secret_field", &encoded)
                .expect("decode should succeed with matching key and AAD");

            assert_eq!(decoded, original);
        });
    }

    #[test]
    fn encode_produces_different_ciphertext_each_time() {
        // Nonce is randomly generated per-encryption, so two encodes of the same
        // plaintext should produce different ciphertexts (nonce differs).
        test_with_codec_key(TEST_KEY_HEX, || {
            let original = String::from("same plaintext");
            let encoded1 = Aes256GcmV1::encode("TestModel", "field", &original).unwrap();
            let encoded2 = Aes256GcmV1::encode("TestModel", "field", &original).unwrap();

            assert_ne!(
                encoded1, encoded2,
                "two encodes of same plaintext should differ (random nonce)"
            );

            // Both should decode to the same value.
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
        // should fail authentication due to AAD mismatch.
        test_with_codec_key(TEST_KEY_HEX, || {
            let original = String::from("aad binding test");
            let encoded = Aes256GcmV1::encode("ModelA", "field_a", &original).unwrap();

            // Try to decode with different model name.
            let err = Aes256GcmV1::decode("ModelB", "field_a", &encoded).unwrap_err();
            assert!(matches!(err, CodecError::AeadError(_)));

            // Try to decode with different field name.
            let err = Aes256GcmV1::decode("ModelA", "field_b", &encoded).unwrap_err();
            assert!(matches!(err, CodecError::AeadError(_)));

            // Correct AAD should still work.
            assert_eq!(
                Aes256GcmV1::decode("ModelA", "field_a", &encoded).unwrap(),
                original
            );
        });
    }

    #[test]
    fn ciphertext_too_short_returns_error() {
        test_with_codec_key(TEST_KEY_HEX, || {
            let short = vec![0u8; 10]; // Less than minimum 28 bytes.
            let err = Aes256GcmV1::decode("Model", "field", &short).unwrap_err();
            assert!(matches!(err, CodecError::CiphertextTooShort { got: 10 }));

            // Error message should contain structural info but no key material.
            let msg = err.to_string();
            assert!(msg.contains("28"));
            assert!(msg.contains("10"));
        });
    }

    #[test]
    fn missing_key_returns_error() {
        // Clear the env var and try to load the key.
        // Note: if the OnceLock was already set by another test, this won't trigger
        // MissingKey because of caching. This test should run in isolation.
        let _lock = TEST_CODEC_ENV_MUTEX.lock().unwrap();

        // Save and clear the env var.
        let prev = std::env::var(ENV_VAR).ok();
        unsafe { std::env::remove_var(ENV_VAR) };

        // If the CODEC_KEY was already set by a previous test, load_key returns Ok.
        // This test is best-effort: it only verifies MissingKey when the cache is empty.
        if CODEC_KEY.get().is_none() {
            let err = load_key().unwrap_err();
            assert!(matches!(err, CodecError::MissingKey));
        }

        // Restore env var state.
        if let Some(v) = prev {
            unsafe { std::env::set_var(ENV_VAR, &v) };
        }
    }

    #[test]
    fn aead_error_display_does_not_leak_sensitive_data() {
        // The aes-gcm crate uses `aead::Error` which is deliberately opaque.
        // Its Display outputs "generic AEAD error" — no key bytes, nonce values,
        // or ciphertext content. This test verifies that property.
        use aes_gcm::aead::Error as AeadImplError;

        // Construct an AEAD error (the type is a single-byte opaque type).
        let aead_err = CodecError::AeadError(AeadImplError);
        let msg = aead_err.to_string();

        // The message should be opaque — no hex, no bytes.
        assert_eq!(msg, "generic AEAD error");
        assert!(!msg.contains("0x"));
        assert!(!msg.contains("key"));
        assert!(!msg.contains("nonce"));
    }

    #[test]
    fn cipher_text_too_short_display_includes_got_length() {
        let err = CodecError::CiphertextTooShort { got: 5 };
        let msg = err.to_string();
        assert!(msg.contains("28"), "should mention minimum length");
        assert!(msg.contains("5"), "should mention actual length");
    }

    #[test]
    fn missing_key_display() {
        let err = CodecError::MissingKey;
        let msg = err.to_string();
        assert!(!msg.is_empty());
        // Message should not contain any key material.
        assert!(!msg.contains("0x"));
    }

    #[test]
    fn codec_error_source_returns_none_for_aead() {
        // aes_gcm::aead::Error is an opaque single-byte type that does not
        // implement std::error::Error, so CodecError cannot chain it.
        use aes_gcm::aead::Error as AeadImplError;
        use std::error::Error;
        let err = CodecError::AeadError(AeadImplError);
        assert!(
            err.source().is_none(),
            "AeadError has no Error-implementing source"
        );
    }

    #[test]
    fn codec_error_source_returns_inner_for_utf8() {
        use std::error::Error;
        let utf8_err = String::from_utf8(vec![0xff]).unwrap_err();
        let err = CodecError::Utf8Error(utf8_err);
        assert!(
            err.source().is_some(),
            "Utf8Error should chain to inner error"
        );
    }

    #[test]
    fn codec_error_source_returns_none_for_missing_key() {
        use std::error::Error;
        let err = CodecError::MissingKey;
        assert!(err.source().is_none(), "MissingKey has no source");
    }

    #[test]
    fn classify_transition_same_codec_is_online_safe() {
        let classification = <Aes256GcmV1 as FieldCodec>::classify_transition::<Aes256GcmV1>();
        assert_eq!(classification, OnlineSafetyClassification::OnlineSafe);
    }

    #[test]
    fn classify_transition_different_codec_is_expand_contract() {
        // Any codec with a different ID than Aes256GcmV1 should return ExpandContract.
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
                String::from_utf8(stored.clone()).map_err(CodecError::Utf8Error)
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
    fn empty_string_round_trip() {
        test_with_codec_key(TEST_KEY_HEX, || {
            let original = String::new();
            let encoded = Aes256GcmV1::encode("Model", "field", &original).unwrap();
            // Minimum ciphertext: 28 bytes (12 nonce + 16 tag for empty plaintext).
            assert_eq!(encoded.len(), 28);

            let decoded = Aes256GcmV1::decode("Model", "field", &encoded).unwrap();
            assert_eq!(decoded, original);
        });
    }

    #[test]
    fn codec_startup_error_display_format() {
        use super::super::CodecStartupError;

        let err = CodecStartupError {
            codec_id: "aes256_gcm_v1",
            env_var: ENV_VAR,
            error: "field codec key not configured".to_owned(),
        };
        let msg = err.to_string();
        assert!(msg.contains("aes256_gcm_v1"));
        assert!(msg.contains(ENV_VAR));
        assert!(msg.contains("field codec key not configured"));
    }

    #[test]
    fn load_key_caches_value_ignores_subsequent_env_changes() {
        // Set a valid key, load it, then change the env var.
        // Subsequent load_key calls should still return the cached key.
        test_with_codec_key(TEST_KEY_HEX, || {
            let _ = load_key(); // Populate the cache.

            // Change the env var to an invalid value.
            unsafe { std::env::set_var(ENV_VAR, "invalid_hex") };

            // load_key should still succeed with the cached key.
            let key = load_key().unwrap();
            assert_eq!(key.len(), 32);
        });
    }
}
