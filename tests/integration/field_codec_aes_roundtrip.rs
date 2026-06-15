// Issue #371 — AES-256-GCM encrypted-at-rest field codec acceptance.
//
// # What this file pins
//
// 1. **Column type override (Class A).** A model field annotated
//  `#[field(protected(codec = "aes256_gcm_v1"))]` projects to a `BYTEA`
//  column — NOT `TEXT`/`VARCHAR` — because the codec's `Encoded` type is
//  `Vec<u8>` regardless of the decoded Rust type (`String`). Verified at the
//  projection layer via `project_from_inventory()` (no DB), and demonstrated
//  end-to-end by the round-trip succeeding (a TEXT column would corrupt the
//  `Vec<u8>` ciphertext bind/decode).
// 2. **CRUD round-trip (Class E).** Create a model with an encrypted `String`
//  field, fetch it back by id, and confirm the plaintext matches — proving
//  encode-on-write and decode-on-read thread the codec through the typed
//  persistence layer.
// 3. **Nullable encrypted field.** `Option<String>` encrypted columns store
//  NULL for `None` (skip encode) and round-trip `Some(value)` through the
//  full encrypt/decrypt path.
//
// # No raw_execute required
//
// Every value the test inserts is reachable through the typed Rust surface
// (`Model::create` / `Model::get` / `project_from_inventory`), so this file
// lives under `tests/integration/` (the raw-free integration target). The codec
// ring is established by setting `DJOGI_FIELD_CODEC_KEY_0` directly — see
// `ensure_ring` — because the in-crate `test_with_codec_ring` helper is
// `#[cfg(test)]` and therefore invisible to this separate integration crate.

#![cfg(feature = "aes-codec")]

use djogi::migrate::projection::{BucketKey, project_from_inventory};
use djogi::prelude::*;

// ── Test model — encrypted String + nullable encrypted String ──────────────

#[model(table = "secret_box_371", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct SecretBox {
    #[field(protected(
        sensitivity = "secret",
        rationale = "encrypted-at-rest test token",
        codec = "aes256_gcm_v1"
    ))]
    pub token: String,
    #[field(protected(
        sensitivity = "secret",
        rationale = "nullable encrypted recovery value",
        codec = "aes256_gcm_v1"
    ))]
    pub recovery: Option<String>,
}

/// A valid 64-lowercase-hex ring entry (32 bytes) for the integration tests.
const RING_KEY_0: &str = "0000000000000000000000000000000000000000000000000000000000000001";

/// Establish the codec ring ONCE per test binary by setting
/// `DJOGI_FIELD_CODEC_KEY_0` directly. The first `encode`/`decode` lazily
/// `load_ring()`s it into the process-global `OnceLock`. A `Once` guard makes
/// this idempotent across the codec tests in this binary (the `OnceLock` is
/// single-set anyway). All codec tests here are `#[serial_test::serial]` because
/// the env var and the ring cache are process-global.
fn ensure_ring() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        // SAFETY: process-global env mutation performed once before any codec
        // CRUD call; the codec tests in this binary are #[serial_test::serial]
        // so no concurrent reader/writer races on env state.
        unsafe { std::env::set_var("DJOGI_FIELD_CODEC_KEY_0", RING_KEY_0) };
    });
}

fn global_key() -> BucketKey {
    BucketKey {
        database: "main".to_string(),
        app: String::new(),
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Class A — codec field projects to a BYTEA column (no DB)
// ───────────────────────────────────────────────────────────────────────────

// `#[djogi::djogi_test]` is the OUTER (top) attribute so it expands first,
// rewriting the async fn into the sync `#[test] fn` harness; `#[serial]` then
// wraps that sync test. The reverse order fails: serial's async path rewrites
// the fn to zero args before djogi_test sees it.
#[djogi::djogi_test(sync_models = [SecretBox])]
#[serial_test::serial]
async fn aes_codec_field_projects_to_bytea_column(_ctx: djogi::DjogiContext) {
    // `project_from_inventory()` is synchronous and safe inside an async body.
    let projected = project_from_inventory()
        .expect("project_from_inventory must succeed for the SecretBox model");
    let global = projected
        .get(&global_key())
        .expect("global bucket is always present");
    let table = global
        .models
        .get("secret_box_371")
        .expect("secret_box_371 table must be projected");

    for col_name in ["token", "recovery"] {
        let col = table
            .columns
            .iter()
            .find(|c| c.name == col_name)
            .unwrap_or_else(|| panic!("column `{col_name}` must be projected"));
        assert_eq!(
            col.sql_type, "BYTEA",
            "encrypted field `{col_name}` must project to BYTEA (codec Encoded = Vec<u8>), \
       not the decoded String type; got `{}`",
            col.sql_type,
        );
        // The codec snapshot field must record the codec on both encrypted
        // columns so the differ can classify codec transitions.
        assert_eq!(
            col.codec.as_deref(),
            Some("aes256_gcm_v1"),
            "encrypted field `{col_name}` must record its codec id in the snapshot",
        );
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Class E — CRUD round-trip: plaintext matches after encrypt → store → decrypt
// ───────────────────────────────────────────────────────────────────────────

#[djogi::djogi_test(sync_models = [SecretBox])]
#[serial_test::serial]
async fn aes_codec_crud_round_trip(mut ctx: djogi::DjogiContext) {
    ensure_ring();

    let created = SecretBox::create(
        &mut ctx,
        SecretBox {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            token: "super-secret-value".to_string(),
            recovery: Some("recovery@example.test".to_string()),
        },
    )
    .await
    .expect("create with encrypted fields must succeed");

    let fetched = SecretBox::get(&mut ctx, created.id)
        .await
        .expect("fetch by id must succeed");

    assert_eq!(
        fetched.token, "super-secret-value",
        "decrypted token must equal the plaintext written",
    );
    assert_eq!(
        fetched.recovery.as_deref(),
        Some("recovery@example.test"),
        "nullable encrypted field must round-trip its Some(value)",
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Class E — nullable encrypted field: None stays NULL and decodes None
// ───────────────────────────────────────────────────────────────────────────

#[djogi::djogi_test(sync_models = [SecretBox])]
#[serial_test::serial]
async fn aes_codec_null_round_trip(mut ctx: djogi::DjogiContext) {
    ensure_ring();

    let created = SecretBox::create(
        &mut ctx,
        SecretBox {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            token: "t".to_string(),
            recovery: None,
        },
    )
    .await
    .expect("create with None recovery must succeed");

    let fetched = SecretBox::get(&mut ctx, created.id)
        .await
        .expect("fetch by id must succeed");

    assert_eq!(fetched.token, "t", "non-null encrypted token round-trips");
    assert_eq!(
        fetched.recovery, None,
        "None recovery must stay NULL and decode back to None",
    );
}
