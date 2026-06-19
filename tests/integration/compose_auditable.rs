// Integration tests: `#[model(auditable)]` opt-in.
//
// What this file pins:
//
// 1. `#[model(auditable)]` emits an `impl ::djogi::Auditable for #ident`
//    block whose `created_by()` getter returns `Option<&str>` borrowed
//    from the adopter-declared `created_by: Option<String>` field.
// 2. The attribute does **not** inject the field — the adopter declares
//    it explicitly (preserved across the surface pivot).
// 3. The `Auditable` trait is convention-sealed only; the macro routes
//    the impl through the public `::djogi::Auditable` re-export, not
//    through `::djogi::__private::*`.
// 4. The macro-emitted `__djogi_auditable_populate` helper runs from
//    `Model::create` between `auto_set_tenant` and the user
//    `before_create` hook. It captures
//    `format!("{}", ctx.auth().user_id)` (Display, not Debug) when
//    auth is present; leaves `created_by = None` when auth is absent
//    (no warn-on-null); never clobbers a user-set
//    value (`if self.created_by.is_none()` guard).
//
// # Surface pivot
//
// `#[model(auditable)]` is the opt-in.
// Tests 1+2 below exercise the success
// cases; tests 3-5 are new.
//
// # One model per test — coherence
//
// `impl Auditable for T` is a coherent impl: only one per `T` per crate.
// Each test therefore declares its own model type sharing a single
// `audit_*` table shape.
//
// # Fixture strategy
//
// Each test provisions its model through `sync_models`, then exercises
// the typed model and trait APIs only.

use djogi::Auditable;
use djogi::auth::AuthContext;
use djogi::prelude::*;

// ---------------------------------------------------------------------------
// Test 1 — `created_by()` returns `Some(&str)` borrowed from the
// in-memory `created_by: Option<String>` field.
//
// The attribute is `#[model(auditable)]`.
// ---------------------------------------------------------------------------

#[model(table = "audit_present", auditable)]
#[derive(Debug, Clone)]
pub struct AuditPresent {
    pub note: String,
    pub created_by: Option<String>,
}

// Serial so it cannot emit tracing into `created_by_null_without_auth`'s
// buffer window; see that test for the rationale.
#[djogi::djogi_test(sync_models = [AuditPresent])]
#[serial_test::serial]
async fn auditable_getter_returns_created_by(mut ctx: djogi::DjogiContext) {
    let row = AuditPresent::create(
        &mut ctx,
        AuditPresent {
            note: "first".into(),
            created_by: Some("alice".into()),
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    // The trait-method call exercises the macro-emitted impl. The
    // `as_deref()` body returns a borrowed `&str` pointing into the
    // in-memory String — no allocation, no copy.
    assert_eq!(
        row.created_by(),
        Some("alice"),
        "Auditable::created_by() must return the adopter-declared field as Option<&str>",
    );

    // Confirm the bound is usable at the framework boundary: code that
    // wants to talk generically about "models with audit metadata" can
    // accept any `M: djogi::Auditable`. If the macro emitted the wrong
    // path (e.g. routed through `__private`), this generic call would
    // still compile but the public `djogi::Auditable` import at the top
    // of the file would be unused — clippy's `unused_imports` lint
    // catches that separately under the workspace `-D warnings`.
    fn read_user<M: djogi::Auditable>(m: &M) -> Option<String> {
        m.created_by().map(str::to_owned)
    }
    assert_eq!(read_user(&row), Some("alice".to_owned()));
}

// ---------------------------------------------------------------------------
// Test 2 — `created_by()` returns `None` when the field is `None` AND
// no `AuthContext` is attached.
//
// This is the pure getter test (no auth attached, no
// user-set value, populator's `is_none()` branch fires but ctx.auth()
// returns None so the field stays None).
// ---------------------------------------------------------------------------

#[model(table = "audit_absent", auditable)]
#[derive(Debug, Clone)]
pub struct AuditAbsent {
    pub note: String,
    pub created_by: Option<String>,
}

// Serial so it cannot emit tracing into `created_by_null_without_auth`'s
// buffer window; see that test for the rationale.
#[djogi::djogi_test(sync_models = [AuditAbsent])]
#[serial_test::serial]
async fn created_by_returns_none_when_unset(mut ctx: djogi::DjogiContext) {
    let row = AuditAbsent::create(
        &mut ctx,
        AuditAbsent {
            note: "second".into(),
            created_by: None,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    assert_eq!(
        row.created_by(),
        None,
        "Auditable::created_by() must return None when the column is NULL — \
         no warn-on-null",
    );
}

// ---------------------------------------------------------------------------
// Test 3 — `created_by` populated from `AuthContext.user_id` when auth
// is attached.
//
// Exercises the `__djogi_auditable_populate` helper. The
// populator uses `Display`, not `Debug`, so the captured string is the
// canonical HeerId format (i64 decimal).
// ---------------------------------------------------------------------------

#[model(table = "audit_with_auth", auditable)]
#[derive(Debug, Clone)]
pub struct AuditWithAuth {
    pub note: String,
    pub created_by: Option<String>,
}

// Serial so it cannot emit tracing into `created_by_null_without_auth`'s
// buffer window; see that test for the rationale.
#[djogi::djogi_test(sync_models = [AuditWithAuth])]
#[serial_test::serial]
async fn created_by_populated_with_auth(mut ctx: djogi::DjogiContext) {
    // Construct a HeerId via `from_i64` so we know the exact Display
    // form. The populator emits `format!("{}", a.user_id)` so the
    // captured string is whatever `<HeerId as Display>::fmt` produces.
    let user_id = HeerId::from_i64(42).expect("valid HeerId");
    let expected = format!("{}", user_id);

    ctx.set_auth(AuthContext::new(user_id));

    let row = AuditWithAuth::create(
        &mut ctx,
        AuditWithAuth {
            note: "auth-driven".into(),
            // Adopter leaves `created_by` as None — populator must
            // capture the value from auth.
            created_by: None,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    assert_eq!(
        row.created_by().map(str::to_owned),
        Some(expected),
        "Auditable populator must capture format!(\"{{}}\", auth.user_id) (Display, not Debug)",
    );
}

// ---------------------------------------------------------------------------
// Test 4 — `created_by` stays `None` when no auth is attached AND no
// `tracing::warn!` is emitted.
//
// Framework-internal contexts (seeds, migrations) run
// without auth and the populator must not emit operational noise.
// ---------------------------------------------------------------------------

#[model(table = "audit_no_auth", auditable)]
#[derive(Debug, Clone)]
pub struct AuditNoAuth {
    pub note: String,
    pub created_by: Option<String>,
}

/// Helper: ensure the `tracing_test` global subscriber is installed and return
/// the byte length of the global log buffer at this point. Mirrors the pattern
/// in `tests/integration/auth.rs::init_log_capture`.
fn init_log_capture() -> usize {
    tracing_test::internal::INITIALIZED.call_once(|| {
        let buf = tracing_test::internal::global_buf();
        let mock_writer = tracing_test::internal::MockWriter::new(buf);
        let subscriber = tracing_test::internal::get_subscriber(mock_writer, "trace");
        // `set_global_default` may silently no-op if a default is already set —
        // the test binary may install one elsewhere. We tolerate that.
        tracing::dispatcher::set_global_default(subscriber).unwrap_or(());
    });
    tracing_test::internal::global_buf().lock().unwrap().len()
}

/// Return the substring of the global log buffer appended since `since`.
fn logs_since(since: usize) -> String {
    let buf = tracing_test::internal::global_buf().lock().unwrap();
    std::str::from_utf8(&buf[since..]).unwrap_or("").to_owned()
}

#[djogi::djogi_test(sync_models = [AuditNoAuth])]
// This test asserts the ABSENCE of any `WARN`-level marker in the
// process-global tracing buffer window it snapshots. That generic-marker
// assertion is only sound if no other test in this binary emits tracing
// concurrently, so every test in this file is `#[serial_test::serial]`
// (default key, forwarded by `#[djogi_test]` onto the generated sync
// wrapper) to restore exclusive-buffer execution after the CI
// `--test-threads=1` flag was removed.
#[serial_test::serial]
async fn created_by_null_without_auth(mut ctx: djogi::DjogiContext) {
    // Snapshot the log buffer BEFORE the create so we only inspect lines
    // emitted by the populator path.
    let since = init_log_capture();

    // No `ctx.set_auth(...)` — `ctx.auth()` returns `None`. The
    // populator must leave `created_by = None` and emit no warn.
    let row = AuditNoAuth::create(
        &mut ctx,
        AuditNoAuth {
            note: "no-auth".into(),
            created_by: None,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    assert_eq!(
        row.created_by(),
        None,
        "populator must leave `created_by = None` when ctx.auth() is None — \
         framework-internal contexts (seeds, migrations) run without auth",
    );

    // Verify no `tracing::warn!` was emitted by the populator path.
    // The check is substring-based: any warn-level event
    // emitted by tracing_test serializes a `WARN` level marker into the
    // mock writer's buffer.
    let new_logs = logs_since(since);
    assert!(
        !new_logs.contains("WARN"),
        "populator must not emit tracing::warn! when auth is absent; got logs: {new_logs}",
    );
}

// ---------------------------------------------------------------------------
// Test 5 — user-set `created_by` is preserved (`is_none()` guard).
//
// When the adopter constructs the model with
// `created_by: Some("override".into())`, the populator's `if
// self.created_by.is_none()` guard short-circuits and the user value
// survives even when auth is attached.
// ---------------------------------------------------------------------------

#[model(table = "audit_override", auditable)]
#[derive(Debug, Clone)]
pub struct AuditOverride {
    pub note: String,
    pub created_by: Option<String>,
}

// Serial so it cannot emit tracing into `created_by_null_without_auth`'s
// buffer window; see that test for the rationale.
#[djogi::djogi_test(sync_models = [AuditOverride])]
#[serial_test::serial]
async fn created_by_user_override_wins(mut ctx: djogi::DjogiContext) {
    // Attach auth so the populator WOULD capture user_id if the guard
    // were missing — this proves the guard, not the absence of auth.
    let user_id = HeerId::from_i64(99).expect("valid HeerId");
    ctx.set_auth(AuthContext::new(user_id));

    let row = AuditOverride::create(
        &mut ctx,
        AuditOverride {
            note: "user-override".into(),
            // User explicitly sets `created_by` — populator must
            // observe `is_some()` and skip.
            created_by: Some("override".into()),
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    assert_eq!(
        row.created_by(),
        Some("override"),
        "populator's `if self.created_by.is_none()` guard is load-bearing — \
         a user-set value must never be clobbered",
    );
}
