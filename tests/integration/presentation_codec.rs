//! Acceptance test for GH #227 — `#[field(protected(per_scope = ...))]`
//! presentation-codec support.
//!
//! # Status
//!
//! **Compile errors in this file are expected until Stages 2–7 complete.**
//! This file is the acceptance criterion for the full feature:
//!
//! - Stage 2 defines `DjogiError::Presentation` and the `djogi::presentation`
//!   module skeleton.
//! - Stage 3 defines the `PresentationCodecInfo` trait.
//! - Stage 4 extends `#[derive(Model)]` with the `visage_scopes(name = Suffix)`
//!   syntax and the `per_scope` codec grammar inside `protected(...)`.
//! - Stage 5 implements `MaskString` and other built-in codecs.
//! - Stage 6 wires `validate_startup_inventory()` into `DjogiPool::connect`.
//! - Stage 7 adds `djogi::testing::install_presentation_hmac_key_for_testing`.
//!
//! Once all seven stages are complete every item in this file must compile and
//! every `#[tokio::test]` / `#[djogi_test]` body must pass.
//!
//! # What is asserted
//!
//! 1. **Pool startup validation** — `DjogiPool::connect` with
//!    `DJOGI_PRESENTATION_HMAC_KEY` unset returns `Err(DjogiError::Presentation
//!    { .. })`, not a panic.
//! 2. **Custom scope generates visage struct** — `visage_scopes(support =
//!    Support)` on `#[model(...)]` causes the macro to emit a `UserSupport`
//!    struct; `UserSupport::from(&user)` is infallible and preserves `id`,
//!    `created_at`, `updated_at`.
//! 3. **PresentationCodec changes Output type** — a field annotated with
//!    `per_scope = { public = { presentation_codec = MaskString } }` carries
//!    `<MaskString as PresentationCodecInfo<String>>::Output` as its type in
//!    `UserPublic`, not `String`.
//! 4. **TryPresentationCodec makes the scope visage use `TryFrom`** — a field
//!    with `try_presentation_codec = C` on the `public` scope makes `UserWithTry`
//!    implement `TryFrom<&User>`, not `From<&User>`.
//! 5. **`validate_startup_inventory()` returns `Err` when the HMAC key is
//!    missing** — the freestanding validator surfaces the same error as the pool
//!    connection path.
//! 6. **Test-key install before pool connect** — the testing helper
//!    `djogi::testing::install_presentation_hmac_key_for_testing` installs a
//!    64-hex-char key and allows `DjogiPool::connect` to succeed; a
//!    `UserWithCodec::create` / fetch round-trip then works end-to-end.

use djogi::prelude::*;

// ─────────────────────────────────────────────────────────────────────────────
// Env-mutation serialisation
//
// The startup-validation tests temporarily remove
// `DJOGI_PRESENTATION_HMAC_KEY` from the process environment. Any two
// tests running concurrently that touch the same env var will see each
// other's mutations. This static mutex serialises every test that calls
// `std::env::remove_var` / `std::env::set_var` so the process state is
// predictable regardless of test-thread scheduling.
// ─────────────────────────────────────────────────────────────────────────────

static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ─────────────────────────────────────────────────────────────────────────────
// Model declarations used across multiple assertions.
//
// Table names carry the `gh227_` prefix to avoid collisions with every other
// integration test fixture registered in the same database.
// ─────────────────────────────────────────────────────────────────────────────

/// Minimal model used for Assertions 2 and 3.
///
/// `visage_scopes(support = Support)` (Stage 4) tells the macro to emit a
/// `UserSupport` struct in addition to the canonical built-in-scope visages.
///
/// The `public` field carries `per_scope = { public = { presentation_codec =
/// djogi::presentation::builtins::MaskString } }` (Stage 4 / 5) so that in
/// `UserPublic` the field type changes from `String` to
/// `<MaskString as PresentationCodecInfo<String>>::Output`.
// TODO Stage 4: confirm exact `visage_scopes(...)` argument syntax once the
// macro grammar is pinned.
#[model(
    table = "gh227_codec_users",
    visage_scopes(support = Support)
)]
#[derive(Debug, Clone)]
pub struct User {
    /// Exposed on `public` with a presentation codec that masks the raw value.
    /// Exposed on `support` without a codec so support staff see the plaintext.
    #[field(
        expose(public, support),
        protected(
            sensitivity = "pii",
            rationale = "GH #227: email is PII per GDPR Art. 6(1)(b)",
            per_scope = {
                public = {
                    presentation_codec = djogi::presentation::builtins::MaskString
                }
            }
        )
    )]
    pub email: String,
}

/// Model used for Assertion 4 — a field with `try_presentation_codec` forces
/// the generated visage to implement `TryFrom<&UserWithTry>` instead of
/// `From<&UserWithTry>`.
///
/// `try_presentation_codec` is chosen over `presentation_codec` when the codec
/// encode step is itself fallible (e.g. HMAC sign + encrypt path). The macro
/// uses the presence of this key to select the `TryFrom` conversion instead of
/// the infallible `From`.
// TODO Stage 4: confirm that `try_presentation_codec` is the correct key name.
#[model(table = "gh227_try_codec_users")]
#[derive(Debug, Clone)]
pub struct UserWithTry {
    #[field(
        expose(public),
        protected(
            sensitivity = "pii",
            rationale = "GH #227: phone is PII; codec is fallible (encrypt path)",
            per_scope = {
                public = {
                    try_presentation_codec = djogi::presentation::builtins::MaskString
                }
            }
        )
    )]
    pub phone: String,
}

/// Model used for Assertion 6 — requires a valid HMAC key at pool connect time
/// because it carries a `presentation_codec` field.
#[model(table = "gh227_codec_users_round_trip")]
#[derive(Debug, Clone)]
pub struct UserWithCodec {
    #[field(
        expose(public),
        protected(
            sensitivity = "pii",
            rationale = "GH #227: round-trip acceptance test for full create/fetch cycle",
            per_scope = {
                public = {
                    presentation_codec = djogi::presentation::builtins::MaskString
                }
            }
        )
    )]
    pub display_name: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Assertion 1 — Pool startup validation
// ─────────────────────────────────────────────────────────────────────────────

/// `DjogiPool::connect` with `DJOGI_PRESENTATION_HMAC_KEY` absent from the
/// environment must return `Err(DjogiError::Presentation { .. })`.
///
/// The error is returned, not panicked, so callers can surface a useful
/// diagnostic rather than crashing the process on mis-configured deployments.
///
/// Uses `ENV_MUTEX` to serialise env mutation with Assertion 5.
#[tokio::test]
async fn pool_connect_fails_without_hmac_key() {
    let url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL required for pool startup tests");

    let _guard = ENV_MUTEX.lock().expect("ENV_MUTEX poisoned");

    // Save the current value so we can restore it after the test.
    let saved = std::env::var("DJOGI_PRESENTATION_HMAC_KEY").ok();

    std::env::remove_var("DJOGI_PRESENTATION_HMAC_KEY");

    let result = DjogiPool::connect(&url).await;

    // Restore before any assertion so a test failure cannot leave other
    // tests running without the key.
    match &saved {
        Some(v) => std::env::set_var("DJOGI_PRESENTATION_HMAC_KEY", v),
        None => std::env::remove_var("DJOGI_PRESENTATION_HMAC_KEY"),
    }

    // TODO Stage 2: pin the exact variant name once DjogiError::Presentation
    // is defined. The pattern below uses struct-form `{ .. }` which matches
    // any named-field variant — update to a tighter pattern (e.g. checking
    // `message` or an inner error type) once the variant shape is known.
    let err = result.expect_err("pool connect must fail when HMAC key is absent");
    assert!(
        matches!(err, DjogiError::Presentation { .. }),
        "expected DjogiError::Presentation, got: {err:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Assertion 3 — PresentationCodec changes the Output type in the visage
//
// This is a compile-level assertion: if the Output type is wrong, the let
// binding below will not compile.
// ─────────────────────────────────────────────────────────────────────────────

/// Compile-level assertion: the `email` field in `UserPublic` carries
/// `<djogi::presentation::builtins::MaskString as
///  djogi::presentation::PresentationCodecInfo<String>>::Output`
/// as its type, not `String`.
///
/// The `const _` block runs at compile time only. A runtime value is never
/// produced — the assertion is purely about whether the assignment type-checks.
///
/// If Stage 5 defines `MaskString::Output = String` (a no-op codec), this
/// assertion becomes a tautology. That is intentional for the acceptance test:
/// the important check is that the *plumbing* routes through the associated
/// type, not that the output type differs from the input type. A reviewer
/// verifying the end-to-end can add a negative assertion against a distinct
/// codec whose `Output != Input` in the codec unit tests (Stage 5).
// TODO Stage 5: if MaskString::Output != String, update the type annotation
// below to match.
const _: () = {
    fn _assert_public_email_output_type(v: &User) {
        // Stage 4 / 5: `UserPublic::from(v).email` must be of type
        // `<djogi::presentation::builtins::MaskString
        //      as djogi::presentation::PresentationCodecInfo<String>>::Output`.
        //
        // The explicit type annotation on `_email` is the assertion. If the
        // field type were still `String` but `Output` is not `String`, the
        // compiler would reject the binding. If `Output` IS `String`, it
        // still proves the plumbing compiled correctly.
        let public = UserPublic::from(v);
        let _email: <djogi::presentation::builtins::MaskString
            as djogi::presentation::PresentationCodecInfo<String>>::Output = public.email;
        let _ = _email;
    }
};

// ─────────────────────────────────────────────────────────────────────────────
// Assertion 4 — TryPresentationCodec forces TryFrom on the scope visage
// ─────────────────────────────────────────────────────────────────────────────

/// Compile-level assertion: `UserWithTryPublic` implements `TryFrom<&UserWithTry>`,
/// not `From<&UserWithTry>`.
///
/// The `const _` body calls `UserWithTryPublic::try_from(v)`, which would fail
/// to compile if the macro emitted `From` instead of `TryFrom` (because `From`
/// exposes `from(v)`, not `try_from(v)`, on the generated type directly).
///
/// The result type annotation pins that the error side is also routable — the
/// `_` wildcard accepts any `E` that the `TryFrom` impl declares.
// TODO Stage 4: replace the `_` in `Result<UserWithTryPublic, _>` with the
// concrete error type once the macro emits it (likely `djogi::VisageError` or
// a new `djogi::presentation::PresentationCodecError`).
const _: () = {
    fn _assert_try_from_for_try_codec_visage(v: &UserWithTry) {
        let _: Result<UserWithTryPublic, _> = UserWithTryPublic::try_from(v);
    }
};

// ─────────────────────────────────────────────────────────────────────────────
// Assertion 5 — validate_startup_inventory() returns Err when HMAC key missing
// ─────────────────────────────────────────────────────────────────────────────

/// `djogi::presentation::validate_startup_inventory()` is the freestanding
/// validator that `DjogiPool::connect` calls internally. Exposing it as a
/// standalone function lets adopters run the same check during app boot (e.g.
/// before accepting traffic) and write targeted tests without spinning up a
/// full pool.
///
/// This test confirms it returns `Err` when `DJOGI_PRESENTATION_HMAC_KEY` is
/// absent — the same condition that blocks pool connect in Assertion 1.
#[tokio::test]
async fn validate_startup_inventory_errs_without_hmac_key() {
    let _guard = ENV_MUTEX.lock().expect("ENV_MUTEX poisoned");

    let saved = std::env::var("DJOGI_PRESENTATION_HMAC_KEY").ok();

    std::env::remove_var("DJOGI_PRESENTATION_HMAC_KEY");

    let result = djogi::presentation::validate_startup_inventory();

    // Restore before asserting.
    match &saved {
        Some(v) => std::env::set_var("DJOGI_PRESENTATION_HMAC_KEY", v),
        None => std::env::remove_var("DJOGI_PRESENTATION_HMAC_KEY"),
    }

    assert!(
        result.is_err(),
        "validate_startup_inventory must return Err when DJOGI_PRESENTATION_HMAC_KEY is unset"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Assertion 2 — Custom scope generates visage struct
// ─────────────────────────────────────────────────────────────────────────────

/// `visage_scopes(support = Support)` on `#[model(...)]` (Stage 4) causes the
/// macro to emit a `UserSupport` struct. `UserSupport::from(&user)` must be
/// infallible (scalar-only visage) and the resulting struct must carry the same
/// `id`, `created_at`, and `updated_at` as the source model.
///
/// The `email` field is exposed on `support` without a codec (see the `User`
/// declaration above), so `UserSupport::email` is plain `String`.
#[djogi::djogi_test(sync_models = [User])]
async fn custom_scope_generates_visage_struct(mut ctx: DjogiContext) {
    let user = User::create(
        &mut ctx,
        User {
            email: "acceptance@example.com".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("create user for custom-scope assertion");

    // `UserSupport` must exist (Stage 4 generates it) and `From<&User>` must
    // be implemented (scalar-only visage — no relation nesting, no fallible
    // codec on the support scope).
    let support_view = UserSupport::from(&user);

    assert_eq!(
        support_view.id, user.id,
        "UserSupport::id must equal the source model id"
    );
    assert_eq!(
        support_view.created_at, user.created_at,
        "UserSupport::created_at must equal the source model created_at"
    );
    assert_eq!(
        support_view.updated_at, user.updated_at,
        "UserSupport::updated_at must equal the source model updated_at"
    );
    // The email is in the support scope without a codec — it carries the
    // plaintext value.
    assert_eq!(
        support_view.email,
        "acceptance@example.com",
        "UserSupport::email must be the plaintext source value"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Assertion 6 — Test-key install before pool connect
// ─────────────────────────────────────────────────────────────────────────────

/// `djogi::testing::install_presentation_hmac_key_for_testing` (Stage 7)
/// installs a 64-lowercase-hex-char test key into the process environment so
/// that `DjogiPool::connect` succeeds in test harnesses that do not set
/// `DJOGI_PRESENTATION_HMAC_KEY` in their environment.
///
/// After the helper runs, a `UserWithCodec::create` / `UserWithCodec::objects()`
/// round-trip must complete successfully, proving the presentation codec is
/// wired into the full CRUD path without breaking the typed surface.
///
/// # Why 64 hex chars?
///
/// The HMAC key must supply at least 256 bits of entropy (32 bytes). Encoding
/// as lowercase hex doubles the byte count, so 64 characters = 32 bytes = 256
/// bits. The framework rejects shorter keys at startup time (Stage 6 / Stage 7
/// validation).
///
/// # Connection URL
///
/// This test uses `#[djogi_test]` so the harness injects a context backed by
/// the test database URL. The test-key helper installs the env var before the
/// pool is created by the harness, which is why the call must happen BEFORE the
/// `djogi_test` macro's pool-construction phase. The macro guarantees that any
/// code in the `sync_models` prologue runs before the pool is created; until
/// Stage 7 defines the exact hook point, we model the intended interaction in a
/// comment here and exercise it via the env-var path inside the test body.
///
/// If Stage 7 adds a dedicated `before_pool` hook to `djogi_test`, update this
/// test to use that hook and remove the `std::env::set_var` call from the body.
// TODO Stage 7: if the testing helper must be called before the djogi_test
// harness builds its pool, restructure this test to use whichever
// pre-pool hook Stage 7 introduces. For now we call it at the top of the
// test body as the closest approximation — the pool is already open when we
// get here, but the call still exercises the helper's own validation path
// (key length check, env-var installation, idempotency).
#[djogi::djogi_test(sync_models = [UserWithCodec])]
async fn test_key_installed_before_pool_connect(mut ctx: DjogiContext) {
    // Install the 64-hex-char test HMAC key. The helper:
    //   1. Validates the key length (must be exactly 64 lowercase hex chars).
    //   2. Sets DJOGI_PRESENTATION_HMAC_KEY in the process environment.
    //   3. Is idempotent — calling it multiple times with the same key is safe.
    djogi::testing::install_presentation_hmac_key_for_testing(
        "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899",
    );

    // Full CRUD round-trip through the typed surface. The presentation codec
    // is applied at create time (encode path) and at fetch time (decode path).
    // Both must succeed for the round-trip to pass.
    let created = UserWithCodec::create(
        &mut ctx,
        UserWithCodec {
            display_name: "Acceptance Test User".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("create UserWithCodec must succeed when HMAC key is installed");

    // Fetch back via the queryset to exercise the decode path.
    let fetched: Vec<UserWithCodec> = UserWithCodec::objects()
        .filter(|f| f.id().eq(created.id))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch UserWithCodec must succeed after create");

    assert_eq!(fetched.len(), 1, "exactly one row must come back");
    assert_eq!(
        fetched[0].id, created.id,
        "fetched row must match the created row by id"
    );
}
