//! HMAC-SHA256 sign + constant-time verify primitives for migration snapshots.
//! # Threat model
//! The signed object is `migrations/schema_snapshot.json` (and, in future
//! tasks, the `djogi_ddl_audit` row payloads). The signature deters two
//! attacker classes:
//! 1. **Filesystem tamper.** An operator (or compromised CI cache) edits the
//!    snapshot to suppress a drift warning or revert a schema change. The
//!    differ would otherwise believe the edited file represents the
//!    schema-of-record and silently mis-classify the next migration. With
//!    signing enabled, the signature on the persisted file no longer
//!    verifies and the differ refuses to trust the snapshot.
//! 2. **Timing side-channel.** A naive verify path that compares signature
//!    bytes with `==` short-circuits on the first mismatching byte, leaking
//!    one byte of secret per probe. We compute the full HMAC, then compare
//!    via [`subtle::ConstantTimeEq`], so any two 32-byte signatures take the
//!    same number of cycles to compare regardless of how many leading bytes
//!    happen to agree.
//! # No-op key sentinel
//! The framework supports unsigned snapshots — dev workflows, ephemeral CI,
//! and any deployment that does not set `DJOGI_SNAPSHOT_SIGNING_KEY` should
//! continue to work without ceremony. We encode "signing disabled" as the
//! all-zeros key `[0u8; 32]`:
//! - [`sign_snapshot`] short-circuits to `[0u8; 32]` — the signature is
//!   never computed, and every snapshot persisted under the sentinel key
//!   carries the same zero-byte signature.
//! - [`verify_snapshot`] returns `true` for the exact pair `(zero_sig,
//! zero_key)` and `false` for any other shape. Crucially, when an
//!   attacker submits a non-zero forged signature against the no-op key,
//!   the comparison still routes through the constant-time path so the
//!   bypass attempt does not leak timing information.
//!   Production deployments that care about integrity simply set
//!   `DJOGI_SNAPSHOT_SIGNING_KEY` to a random 32-byte hex value. The same
//!   key must round-trip to the verifier; key rotation is a future-task
//!   concern.
//!   See the parent module for the higher-level snapshot integrity story.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::env;
use subtle::ConstantTimeEq;

// The all-zero sentinel `[0u8; 32]` denotes "signing disabled".
// HMAC-SHA256 over real input has a ~2^-256 probability of producing
// this output — vanishingly small (lower than a hash collision in
// the lifetime of the universe). The sentinel is therefore safe
// even though it occupies a value in the legitimate output range.

/// Signing-disabled sentinel. Per the module-level "No-op key sentinel"
/// section, all-zeros means "do not compute or verify HMAC".
const NOOP_KEY: [u8; 32] = [0u8; 32];

/// Zero-byte signature emitted under the no-op key. Verifying any other
/// signature against the no-op key returns `false`.
const NOOP_SIG: [u8; 32] = [0u8; 32];

/// HMAC-SHA256 type alias used for snapshot signing.
type HmacSha256 = Hmac<Sha256>;

#[cfg(test)]
pub(crate) static SIGNING_KEY_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Sign `json_bytes` with `key` using HMAC-SHA256.
/// Returns the 32-byte MAC over the input bytes. The caller is responsible
/// for serialising the snapshot to a canonical byte form before calling
/// (typically `serde_json::to_vec_pretty(&snapshot)`).
/// # No-op key sentinel
/// When `key == [0u8; 32]` (the "signing disabled" default), the function
/// short-circuits to `[0u8; 32]` without invoking the HMAC primitive. This
/// makes `DJOGI_SNAPSHOT_SIGNING_KEY` opt-in: dev and CI runs that don't
/// set the env var produce zero-signature snapshots that round-trip
/// cleanly via [`verify_snapshot`].
pub fn sign_snapshot(json_bytes: &[u8], key: &[u8; 32]) -> [u8; 32] {
    if key == &NOOP_KEY {
        return NOOP_SIG;
    }

    // `Hmac::new_from_slice` only fails for KEY-length-mismatch errors on
    // ciphers that constrain key length; HMAC accepts any byte length so
    // this never errors in practice. `expect` documents the invariant
    // rather than papering over a real failure mode.
    let mut mac = HmacSha256::new_from_slice(key)
        .expect("HMAC accepts any key length; passing &[u8; 32] cannot fail");
    mac.update(json_bytes);
    let result = mac.finalize().into_bytes();

    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

/// Verify `signature` against HMAC-SHA256 of `json_bytes` under `key`.
/// Returns `true` if the signature is authentic, `false` otherwise.
/// # Constant-time comparison
/// The actual byte comparison routes through [`subtle::ConstantTimeEq`]
/// the function never short-circuits on a leading-byte mismatch and never
/// uses `==` on signature bytes. This prevents timing-side-channel leaks
/// of the secret signature material.
/// # No-op key sentinel
/// - When `key == [0u8; 32]` AND `signature == [0u8; 32]`, returns `true`
///   (matches the [`sign_snapshot`] no-op path).
/// - When `key == [0u8; 32]` AND `signature != [0u8; 32]`, the constant-time
///   comparison still runs against the zero signature and returns `false`.
///   An attacker forging a non-zero signature against the no-op key cannot
///   short-circuit verification or leak timing information.
pub fn verify_snapshot(json_bytes: &[u8], signature: &[u8; 32], key: &[u8; 32]) -> bool {
    let expected = sign_snapshot(json_bytes, key);
    // `subtle::Choice` -> `bool` via `bool::from` keeps the comparison
    // explicitly constant-time. Do NOT replace with `expected == *signature`;
    // PartialEq for arrays short-circuits on first byte mismatch.
    bool::from(expected.ct_eq(signature))
}

/// Read `DJOGI_SNAPSHOT_SIGNING_KEY` from the environment and decode it as
/// a 32-byte HMAC key.
/// The env var must be **exactly 64 hex characters**, lowercase or uppercase
/// (`0`–`9`, `a`–`f`, `A`–`F`). Any other length or out-of-range byte yields
/// a [`SnapshotKeyError`]. When the variable is unset, returns `Ok(None)`
/// callers treat that as "use the no-op key sentinel".
/// # Implementation notes
/// Hex parsing is hand-rolled via a byte loop; per the djogi-wide rule
/// (`feedback_no_regex_in_djogi.md`) and to avoid a transitive dependency
/// on the `hex` crate for two dozen lines of decode logic. Match arms cover
/// the three ASCII ranges explicitly and reject everything else.
pub fn load_signing_key_from_env() -> Result<Option<[u8; 32]>, SnapshotKeyError> {
    let raw = match env::var("DJOGI_SNAPSHOT_SIGNING_KEY") {
        Ok(s) => s,
        // Unset → caller falls back to the no-op key sentinel.
        Err(env::VarError::NotPresent) => return Ok(None),
        // Set but non-UTF-8 → surface as a hard error rather than
        // silently degrading to the no-op sentinel. Collapsing this
        // into `Ok(None)` would let a malformed env var (typo,
        // accidental binary paste, injection) downgrade signing to a
        // no-op without warning, which violates the documented
        // signing contract. The operator must explicitly fix or
        // unset the variable.
        Err(env::VarError::NotUnicode(_)) => return Err(SnapshotKeyError::NonUnicodeEnvVar),
    };

    let bytes = raw.as_bytes();
    if bytes.len() != 64 {
        return Err(SnapshotKeyError::InvalidLength {
            actual: bytes.len(),
        });
    }

    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        let hi = decode_hex_nibble(bytes[i * 2], i * 2)?;
        let lo = decode_hex_nibble(bytes[i * 2 + 1], i * 2 + 1)?;
        out[i] = (hi << 4) | lo;
        i += 1;
    }
    Ok(Some(out))
}

/// Decode a single ASCII hex character into its 4-bit nibble. Accepts
/// lowercase `a`–`f`, uppercase `A`–`F`, and digits `0`–`9`. Anything else
/// returns [`SnapshotKeyError::InvalidHexByte`] tagged with the original
/// byte's index in the input string.
fn decode_hex_nibble(byte: u8, idx: usize) -> Result<u8, SnapshotKeyError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(SnapshotKeyError::InvalidHexByte { idx, byte }),
    }
}

/// Errors surfaced by [`load_signing_key_from_env`].
/// The variants are deliberately narrow — the failure modes are wrong-length
/// input, a stray non-hex byte, and a non-UTF-8 env-var value. We hand-roll
/// `Display` and `Error` rather than pulling `thiserror` in for three
/// variants; the message format is stable and the boilerplate is trivial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotKeyError {
    /// The env var was set but its byte length was not exactly 64. The
    /// `actual` field reports the observed length so log lines can help
    /// operators diagnose copy-paste truncation or extra whitespace.
    InvalidLength { actual: usize },
    /// A byte at index `idx` was not a valid hex digit. `byte` carries the
    /// offending byte for diagnostics; the framework does not log the
    /// surrounding bytes (which together with `idx` could leak partial
    /// secret material — see the verify-side timing-channel rationale at
    /// the top of this module).
    InvalidHexByte { idx: usize, byte: u8 },
    /// `DJOGI_SNAPSHOT_SIGNING_KEY` is set but contains non-UTF-8 bytes.
    /// Treating this as "no key set" would silently degrade signing to
    /// the no-op sentinel — a security regression. Surface as an error
    /// so the operator must explicitly fix the env var or unset it.
    NonUnicodeEnvVar,
}

impl std::fmt::Display for SnapshotKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotKeyError::InvalidLength { actual } => write!(
                f,
                "DJOGI_SNAPSHOT_SIGNING_KEY must be exactly 64 hex characters (got {actual})",
            ),
            SnapshotKeyError::InvalidHexByte { idx, byte } => write!(
                f,
                "DJOGI_SNAPSHOT_SIGNING_KEY contains a non-hex byte at index {idx} (byte 0x{byte:02x})",
            ),
            SnapshotKeyError::NonUnicodeEnvVar => write!(
                f,
                "DJOGI_SNAPSHOT_SIGNING_KEY is set but contains non-UTF-8 bytes; \
                 fix the value or unset the variable to disable signing",
            ),
        }
    }
}

impl std::error::Error for SnapshotKeyError {}

#[cfg(test)]
mod tests {
    use super::*;

    // The process-wide mutex lives outside this test module so
    // `migrate::runner` tests can share the same env-var guard. `cargo test`
    // runs unit tests in parallel by default; every test in this crate that
    // reads or mutates `DJOGI_SNAPSHOT_SIGNING_KEY` must hold the mutex.

    /// Pinned HMAC-SHA256 test vector. Not an RFC-4231 case (those use
    /// variable-length keys; our API is fixed `[u8; 32]`); this vector is
    /// self-consistent and reproducible via:
    /// ```sh
    /// printf 'Hi There' | openssl dgst -sha256 \
    ///     -mac HMAC -macopt hexkey:0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b
    /// ```
    /// The 32-byte key is twenty `0x0b` bytes (RFC-4231 vector 1's key
    /// material) zero-padded to 32 bytes. The data is `b"Hi There"`.
    /// Expected MAC pinned below.
    #[test]
    fn sign_known_input_known_key() {
        let mut key = [0u8; 32];
        for slot in key.iter_mut().take(20) {
            *slot = 0x0b;
        }
        let data = b"Hi There";
        // Generated via the openssl one-liner in the doc comment above
        // and pinned here. Any future libraries swap that breaks HMAC
        // semantics will fail this test loudly.
        // Note: the canonical RFC-4231 Test Case 1 uses a 20-byte key
        // (only the leading 0x0b bytes). This fixture zero-pads that
        // key to 32 bytes — but the resulting MAC is IDENTICAL to the
        // RFC vector. HMAC's key-derivation step pads any key shorter
        // than the block size (64 bytes for SHA-256) with zeros before
        // XOR'ing with the inner/outer pad constants, so a 20-byte key
        // padded to 32 bytes and then internally padded to 64 produces
        // the same `K XOR ipad`/`K XOR opad` as the 20-byte key padded
        // directly to 64. The pinned bytes below match RFC-4231 Test
        // Case 1's expected output and were cross-checked via the
        // openssl command in the doc comment.
        let expected: [u8; 32] = [
            0xb0, 0x34, 0x4c, 0x61, 0xd8, 0xdb, 0x38, 0x53, 0x5c, 0xa8, 0xaf, 0xce, 0xaf, 0x0b,
            0xf1, 0x2b, 0x88, 0x1d, 0xc2, 0x00, 0xc9, 0x83, 0x3d, 0xa7, 0x26, 0xe9, 0x37, 0x6c,
            0x2e, 0x32, 0xcf, 0xf7,
        ];
        let actual = sign_snapshot(data, &key);
        assert_eq!(
            actual, expected,
            "HMAC-SHA256 output drifted from pinned vector"
        );
    }

    #[test]
    fn verify_round_trip_succeeds() {
        let key = [1u8; 32];
        let payload = b"{\"models\":[]}";
        let sig = sign_snapshot(payload, &key);
        assert!(
            verify_snapshot(payload, &sig, &key),
            "freshly-signed payload must verify under the same key",
        );
    }

    #[test]
    fn verify_rejects_tampered_payload() {
        let key = [1u8; 32];
        let payload: &[u8] = b"{\"models\":[]}";
        let sig = sign_snapshot(payload, &key);

        let mut tampered = payload.to_vec();
        tampered[0] ^= 0x01; // flip one bit
        assert!(
            !verify_snapshot(&tampered, &sig, &key),
            "tampered payload must not verify against the original signature",
        );
    }

    #[test]
    fn verify_rejects_tampered_signature() {
        let key = [1u8; 32];
        let payload = b"{\"models\":[]}";
        let mut sig = sign_snapshot(payload, &key);
        sig[0] ^= 0x01; // flip one bit
        assert!(
            !verify_snapshot(payload, &sig, &key),
            "tampered signature must not verify",
        );
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let signing_key = [1u8; 32];
        let verifying_key = [2u8; 32];
        let payload = b"{\"models\":[]}";
        let sig = sign_snapshot(payload, &signing_key);
        assert!(
            !verify_snapshot(payload, &sig, &verifying_key),
            "signature must not verify under a different key",
        );
    }

    #[test]
    fn noop_key_signs_to_zero() {
        let payload = b"{\"models\":[]}";
        let sig = sign_snapshot(payload, &[0u8; 32]);
        assert_eq!(
            sig, [0u8; 32],
            "no-op key must short-circuit to zero signature",
        );
    }

    #[test]
    fn noop_key_zero_sig_verifies() {
        let payload = b"{\"models\":[]}";
        assert!(
            verify_snapshot(payload, &[0u8; 32], &[0u8; 32]),
            "(zero-key, zero-sig) must verify cleanly — round-trip of the no-op path",
        );
    }

    #[test]
    fn noop_key_nonzero_sig_does_not_verify() {
        let payload = b"{\"models\":[]}";
        let forged_sig = [1u8; 32];
        assert!(
            !verify_snapshot(payload, &forged_sig, &[0u8; 32]),
            "non-zero forged signature must not bypass the no-op path",
        );
    }

    #[serial_test::serial]
    #[test]
    fn load_key_from_env_unset() {
        let _g = SIGNING_KEY_ENV_MUTEX.lock().unwrap();
        // SAFETY: `remove_var` is `unsafe` on Rust 2024+ because mutating
        // the process environment from one thread while another reads it
        // is UB on some platforms. `SIGNING_KEY_ENV_MUTEX` serialises every
        // test in this crate that touches `DJOGI_SNAPSHOT_SIGNING_KEY`, so
        // within this test binary no concurrent reader exists.
        unsafe {
            env::remove_var("DJOGI_SNAPSHOT_SIGNING_KEY");
        }
        assert_eq!(load_signing_key_from_env(), Ok(None));
    }

    #[serial_test::serial]
    #[test]
    fn load_key_from_env_valid_hex() {
        let _g = SIGNING_KEY_ENV_MUTEX.lock().unwrap();
        // 64 hex chars = 32 bytes: 0x00, 0x11, 0x22, ..., 0xff (each byte
        // doubled). Mixes lower- and upper-case to exercise both arms of
        // the hex decoder.
        let hex = "00112233445566778899AABBCCDDEEFF00112233445566778899aabbccddeeff";
        // SAFETY: see `load_key_from_env_unset` for the threading argument;
        // `SIGNING_KEY_ENV_MUTEX` serialises all env-var readers and mutators
        // in this crate.
        unsafe {
            env::set_var("DJOGI_SNAPSHOT_SIGNING_KEY", hex);
        }
        let result = load_signing_key_from_env();
        // SAFETY: same as above — clear after the test so adjacent tests
        // see an unset variable.
        unsafe {
            env::remove_var("DJOGI_SNAPSHOT_SIGNING_KEY");
        }
        let expected: [u8; 32] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
            0xcc, 0xdd, 0xee, 0xff,
        ];
        assert_eq!(result, Ok(Some(expected)));
    }

    #[serial_test::serial]
    #[test]
    fn load_key_from_env_short() {
        let _g = SIGNING_KEY_ENV_MUTEX.lock().unwrap();
        // 63 chars — one short of the required 64.
        let hex = "00112233445566778899AABBCCDDEEFF00112233445566778899aabbccddeef";
        // SAFETY: `SIGNING_KEY_ENV_MUTEX` serialises env-var access in this crate.
        unsafe {
            env::set_var("DJOGI_SNAPSHOT_SIGNING_KEY", hex);
        }
        let result = load_signing_key_from_env();
        // SAFETY: same.
        unsafe {
            env::remove_var("DJOGI_SNAPSHOT_SIGNING_KEY");
        }
        assert_eq!(result, Err(SnapshotKeyError::InvalidLength { actual: 63 }),);
    }

    #[serial_test::serial]
    #[test]
    fn load_key_from_env_non_hex() {
        let _g = SIGNING_KEY_ENV_MUTEX.lock().unwrap();
        // 64 chars but with a `g` at index 0 — outside hex range.
        let hex = "g0112233445566778899AABBCCDDEEFF00112233445566778899aabbccddeeff";
        // SAFETY: `SIGNING_KEY_ENV_MUTEX` serialises env-var access in this crate.
        unsafe {
            env::set_var("DJOGI_SNAPSHOT_SIGNING_KEY", hex);
        }
        let result = load_signing_key_from_env();
        // SAFETY: same.
        unsafe {
            env::remove_var("DJOGI_SNAPSHOT_SIGNING_KEY");
        }
        assert_eq!(
            result,
            Err(SnapshotKeyError::InvalidHexByte { idx: 0, byte: b'g' }),
        );
    }

    /// `env::var` distinguishes `NotPresent` (variable is unset) from
    /// `NotUnicode` (variable is set but the value is not valid UTF-8).
    /// The function under test must surface the latter as an error rather
    /// than silently degrading to the no-op key sentinel — otherwise a
    /// malformed env var (typo, accidental binary paste, injection)
    /// downgrades signing to a no-op without warning.
    /// Test is Unix-gated because constructing a non-UTF-8 `OsString`
    /// requires `os::unix::ffi::OsStringExt::from_vec`. On Windows the
    /// equivalent shape (ill-formed UTF-16 in the environment block) is
    /// possible but reaches the same code path; covering it on Unix is
    /// sufficient for the variant-routing assertion.
    #[cfg(unix)]
    #[serial_test::serial]
    #[test]
    fn load_key_from_env_non_unicode_returns_error() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let _g = SIGNING_KEY_ENV_MUTEX.lock().unwrap();
        // SAFETY: SIGNING_KEY_ENV_MUTEX serialises every env-var reader and
        // mutator in this crate; the runtime is single-threaded inside this
        // critical section.
        unsafe {
            env::set_var(
                "DJOGI_SNAPSHOT_SIGNING_KEY",
                OsString::from_vec(vec![0xFF, 0xFE, 0xFD]),
            );
        }
        let result = load_signing_key_from_env();
        // SAFETY: same — clear after the test so adjacent tests see an
        // unset variable.
        unsafe {
            env::remove_var("DJOGI_SNAPSHOT_SIGNING_KEY");
        }
        assert_eq!(result, Err(SnapshotKeyError::NonUnicodeEnvVar));
    }
}
