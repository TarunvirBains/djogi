//! Phase 8ε T9.7 — snapshot signing round-trip integration tests.
//!
//! # Scope
//!
//! These are pure unit-style tests that exercise the public
//! `djogi::snapshot::sign` surface (`sign_snapshot`,
//! `verify_snapshot`) end-to-end with realistic JSON payloads. They
//! do NOT touch a database — the in-crate unit tests in
//! `djogi/src/snapshot/sign.rs::tests` already cover the same
//! contract; this file lives in the integration suite so the public
//! surface is exercised from outside the crate, catching any
//! accidental crate-private regression.
//!
//! The test bodies use realistic JSON shapes (a small fake snapshot
//! with `version` + `models` keys) rather than the trivial
//! `b"hello world"` payload the unit tests pin via the RFC-4231
//! vector. The point is end-to-end coverage of the API at the byte
//! granularity an adopter would hit, not re-running the HMAC-SHA256
//! KAT (which is already pinned in-crate).
//!
//! # Why no `#[djogi_test]`
//!
//! The signing primitives are purely byte-level — no DB, no async, no
//! pool. We use plain `#[test]` rather than `#[djogi::djogi_test]` so
//! the suite runs without a Postgres connection, matching the
//! contract documented in
//! `djogi/src/snapshot/sign.rs::tests` (which is also `#[test]`-only
//! for the round-trip path; only the env-var tests serialise on a
//! mutex).
//!
//! # Spec / memory anchors
//!
//! - v3 plan §452 (snapshot signing surface), §456–462 (T9 cluster
//!   contract), §710–712, §729 — T9.7 brief.
//! - Plan §T9.7 (`docs/superpowers/plans/granular-phase8/cluster-8epsilon-granular.md`).

use djogi::snapshot::sign::{sign_snapshot, verify_snapshot};

/// A 1 KB-ish JSON payload that mimics the real snapshot shape. The
/// surface here is incidental — the test only needs a non-trivial
/// byte string. Pinning the literal lets the round-trip test stay
/// reproducible across rustc upgrades; if the test starts depending
/// on `serde_json::to_vec_pretty` we'd lose byte-stability.
fn fixture_snapshot_bytes() -> Vec<u8> {
    let payload = br#"{
  "snapshot": {
    "version": 1,
    "generated_at": "2026-05-05T00:00:00Z"
  },
  "models": [
    {
      "table": "phase8_widgets",
      "fields": [
        {"name": "id", "ty": "BIGINT", "nullable": false},
        {"name": "name", "ty": "TEXT", "nullable": false},
        {"name": "created_at", "ty": "TIMESTAMPTZ", "nullable": false}
      ]
    },
    {
      "table": "phase8_categories",
      "fields": [
        {"name": "id", "ty": "BIGINT", "nullable": false},
        {"name": "label", "ty": "TEXT", "nullable": false}
      ]
    }
  ]
}
"#;
    payload.to_vec()
}

#[test]
fn sign_then_verify_known_key() {
    // Realistic 1 KB JSON; non-zero key. The round-trip MUST
    // succeed under HMAC-SHA256.
    let payload = fixture_snapshot_bytes();
    let key = [0x01u8; 32];
    let sig = sign_snapshot(&payload, &key);

    // Sanity — the no-op sentinel `[0u8; 32]` is reserved for the
    // signing-disabled path. A legitimate non-zero key MUST NOT
    // collide with it (probability is ~2^-256, negligible, but we
    // pin the negative for doc value).
    assert_ne!(
        sig, [0u8; 32],
        "non-zero key + non-empty payload must not produce the no-op sentinel signature",
    );

    assert!(
        verify_snapshot(&payload, &sig, &key),
        "freshly-signed payload must verify under the same key",
    );
}

#[test]
fn tampered_payload_fails_verification() {
    // Sign the original payload, flip one byte, verify against the
    // tampered payload using the original signature → must fail.
    // This is the canonical filesystem-tamper attack the
    // sign/verify pair is designed to detect.
    let payload = fixture_snapshot_bytes();
    let key = [0x01u8; 32];
    let sig = sign_snapshot(&payload, &key);

    // Construct a tampered payload — swap `"version": 1` for
    // `"version": 2`, simulating an operator (or CI cache) editing
    // the snapshot to revert a schema change.
    let tampered = String::from_utf8(payload.clone())
        .expect("fixture payload is valid UTF-8")
        .replacen("\"version\": 1", "\"version\": 2", 1)
        .into_bytes();
    assert_ne!(
        tampered, payload,
        "fixture must actually tamper the payload",
    );

    assert!(
        !verify_snapshot(&tampered, &sig, &key),
        "tampered payload must NOT verify against the original signature",
    );
}

#[test]
fn tampered_signature_fails_verification() {
    // Original payload, original key; flip one byte of the
    // signature → must fail. This is the canonical
    // signature-replacement attack — an attacker who can edit the
    // ledger row but not recompute the HMAC under the operator's
    // key.
    let payload = fixture_snapshot_bytes();
    let key = [0x01u8; 32];
    let mut sig = sign_snapshot(&payload, &key);
    sig[0] ^= 0x01;

    assert!(
        !verify_snapshot(&payload, &sig, &key),
        "tampered signature must NOT verify against the original payload",
    );
}

#[test]
fn wrong_key_fails_verification() {
    // Sign under one key, verify under another → must fail. This
    // is the canonical wrong-secret attack — the verifier holds the
    // operator's key, the attacker holds a different (random) key.
    let payload = fixture_snapshot_bytes();
    let signing_key = [0x01u8; 32];
    let verifying_key = [0x02u8; 32];
    let sig = sign_snapshot(&payload, &signing_key);

    assert!(
        !verify_snapshot(&payload, &sig, &verifying_key),
        "signature must NOT verify under a different key",
    );
}

#[test]
fn noop_key_round_trip_zero_signature() {
    // No-op sentinel — `[0u8; 32]` key signs every payload to
    // `[0u8; 32]` and verifies cleanly. This is the dev/CI
    // "signing disabled" path; mismatching it would either lock
    // adopters out of the framework or silently degrade signing.
    let payload = fixture_snapshot_bytes();
    let noop_key = [0u8; 32];
    let sig = sign_snapshot(&payload, &noop_key);
    assert_eq!(
        sig, [0u8; 32],
        "no-op key must short-circuit to the zero signature",
    );
    assert!(
        verify_snapshot(&payload, &sig, &noop_key),
        "(zero-key, zero-sig) MUST round-trip cleanly",
    );
}

#[test]
fn noop_key_rejects_forged_nonzero_signature() {
    // No-op sentinel + non-zero forged signature → must fail. An
    // attacker who guesses the no-op key cannot bypass verification
    // by also submitting an arbitrary non-zero signature; the
    // constant-time comparison routes through the zero signature
    // and rejects.
    let payload = fixture_snapshot_bytes();
    let noop_key = [0u8; 32];
    let forged = [0x42u8; 32];

    assert!(
        !verify_snapshot(&payload, &forged, &noop_key),
        "non-zero forged signature must NOT bypass the no-op key path",
    );
}
