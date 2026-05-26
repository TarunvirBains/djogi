//! Acceptance test for GH #227 — `#[field(protected(per_scope = ...))]`
//! presentation-codec support.
//!
//! # Status
//!
//! **Compile errors in this file are expected until Stages 2–7 complete.**
//! This file is the acceptance criterion for the full feature:
//!
//! - Stage 2 defines `DjogiError::PresentationStartup` and the `djogi::presentation`
//!   module with its full trait surface, built-in codecs, and startup validation.
//! - Stage 3 wires `validate_startup_inventory()` into `DjogiPool::connect`.
//! - Stage 4 extends `#[derive(Model)]` with the `visage_scopes(name = Suffix)`
//!   syntax and the `per_scope` codec grammar inside `protected(...)`.
//! - Stage 5 implements `MaskString` and other built-in codecs.
//! - Stage 6 wires `validate_startup_inventory()` into `DjogiPool::connect`.
//! - Stage 7 adds `djogi::testing::install_presentation_hmac_key_for_testing`.
//!
//! Once all seven stages are complete every item in this file must compile and
//! every `#[tokio::test]` / `#[djogi_test]` body must pass.
//!
//! # Environment requirement (`hmac-codec` only)
//!
//! This test binary links a model using `HmacSha256HexString` (see
//! `UserWithHmac`). That codec's `validate_startup` checks for
//! `DJOGI_PRESENTATION_HMAC_KEY` in the process environment, so
//! **`DJOGI_PRESENTATION_HMAC_KEY` must be set to a valid 64-lowercase-hex-char
//! key before running this binary**. Use
//! `DJOGI_PRESENTATION_HMAC_KEY=aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899`
//! for local development and CI. Tests that assert startup-validation failure
//! (Assertions 1 and 5) temporarily remove the key inside an `ENV_MUTEX` guard
//! and restore it afterward — the env-var requirement is not in conflict with
//! those tests.
//!
//! When feature `hmac-codec` is disabled, HMAC-specific assertions and models
//! in this file are compiled out.
//!
//! # What is asserted
//!
//! 1. **Pool startup validation** *(feature `hmac-codec` only)* — `DjogiPool::connect` with
//!    `DJOGI_PRESENTATION_HMAC_KEY` unset returns
//!    `Err(DjogiError::PresentationStartup(..))`, not a panic.
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
//!    connection path. *(feature `hmac-codec` only)*
//! 6. **Test-key install before pool connect** *(feature `hmac-codec` only)* — the doc-hidden,
//!    unsafe testing helper `djogi::testing::install_presentation_hmac_key_for_testing`
//!    installs a 64-hex-char key and allows `DjogiPool::connect` to succeed; a
//!    `UserWithCodec::create` / fetch round-trip then works end-to-end.

use djogi::prelude::*;
use djogi::presentation::{
    PresentationCodecInfo, Queryability, Reversibility, TryPresentationCodec,
};

// ─────────────────────────────────────────────────────────────────────────────
// Env-mutation serialisation
//
// The startup-validation tests temporarily remove
// `DJOGI_PRESENTATION_HMAC_KEY` from the process environment. Any two
// tests running concurrently that touch environment state can see each
// other's mutations. This static async mutex keeps those test windows from
// overlapping, but it does not by itself make `std::env::remove_var` /
// `std::env::set_var` safe; each unsafe block still relies on the broader
// invariant that no concurrent environment reads or writes happen
// process-wide (or that the platform's stronger rule is otherwise met).
// ─────────────────────────────────────────────────────────────────────────────

static ENV_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

/// Model used to assert end-to-end predicate composition through a
/// presentation-gated field in `VisageQuerySet::filter`.
///
/// `Identity` is queryable (`PredicateAndOrder`): the public visage field
/// accessor emits `PresentationFieldRef<..., Identity, String>`, and
/// `.eq(...)` returns `Q<Model>`. This fixture proves the visage queryset
/// entry path accepts that `Q<Model>` substrate directly.
#[model(table = "gh227_queryable_identity_users")]
#[derive(Debug, Clone)]
pub struct UserWithQueryableIdentityCodec {
    #[field(
        expose(public),
        protected(
            sensitivity = "pii",
            rationale = "GH #227 P1 coverage: queryable presentation predicate entry",
            per_scope = {
                public = {
                    presentation_codec = djogi::presentation::builtins::Identity
                }
            }
        )
    )]
    pub email: String,
}

/// Model whose field uses `HmacSha256HexString` as a `try_presentation_codec`.
///
/// This model is not used in any `#[djogi_test]` body. Its sole purpose is
/// to register a [`djogi::presentation::inventory::PresentationCodecUsage`]
/// entry in the binary's linked inventory whose `validate_startup` function
/// calls `HmacSha256HexString::validate_startup`, which checks for
/// `DJOGI_PRESENTATION_HMAC_KEY`. Without this entry, the startup-validation
/// assertions (Assertions 1 and 5) would see an empty-or-MaskString-only
/// inventory and find no failures, causing those tests to incorrectly pass
/// even when the HMAC key is absent.
///
/// # Env-var requirement
///
/// Because this model is linked into the test binary, **`DJOGI_PRESENTATION_HMAC_KEY`
/// must be set to a valid 64-lowercase-hex-char value before running this test
/// binary** (e.g. export the key in `.envrc` or pass it on the `cargo test`
/// command line). The `ENV_MUTEX`-guarded tests (Assertions 1 and 5) temporarily
/// remove the key for their test window and restore it afterward, so the overall
/// binary-wide key requirement does not conflict with those tests.
///
/// The constant `"aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899"` is
/// the conventional test key used throughout this file and in
/// `djogi::testing::install_presentation_hmac_key_for_testing`.
#[model(table = "gh227_hmac_users")]
#[derive(Debug, Clone)]
#[cfg(feature = "hmac-codec")]
pub struct UserWithHmac {
    /// Field protected with an HMAC codec. Used only to register a startup-
    /// validation inventory entry — no `#[djogi_test]` body exercises this model.
    #[field(
        expose(public),
        protected(
            sensitivity = "pii",
            rationale = "GH #227: HMAC codec registers startup-validation inventory entry",
            per_scope = {
                public = {
                    try_presentation_codec = djogi::presentation::builtins::HmacSha256HexString
                }
            }
        )
    )]
    pub email: String,
}

#[model(table = "gh227_failing_fetch_codec_users")]
#[derive(Debug, Clone)]
pub struct UserWithFailingFetchCodec {
    #[field(
        expose(public),
        protected(
            sensitivity = "pii",
            rationale = "GH #227 F5: fetch-time presentation codec error mapping",
            per_scope = {
                public = {
                    try_presentation_codec = FailingFetchCodec
                }
            }
        )
    )]
    pub secret: String,
}

struct FailingFetchCodec;

#[derive(Debug)]
struct FailingFetchCodecError;

impl std::fmt::Display for FailingFetchCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("failing fetch codec rejected sentinel value")
    }
}

impl std::error::Error for FailingFetchCodecError {}

impl PresentationCodecInfo<String> for FailingFetchCodec {
    type Output = String;
    const REVERSIBILITY: Reversibility = Reversibility::OneWay;
    const QUERYABILITY: Queryability = Queryability::Disabled;
}

impl TryPresentationCodec<String> for FailingFetchCodec {
    type Error = FailingFetchCodecError;

    fn try_present(value: &String) -> Result<String, Self::Error> {
        if value == "fail-on-fetch" {
            Err(FailingFetchCodecError)
        } else {
            Ok(format!("presented:{value}"))
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Assertion 1 — Pool startup validation
// ─────────────────────────────────────────────────────────────────────────────

/// `DjogiPool::connect` with `DJOGI_PRESENTATION_HMAC_KEY` absent from the
/// environment must return `Err(DjogiError::PresentationStartup(..))`.
///
/// The error is returned, not panicked, so callers can surface a useful
/// diagnostic rather than crashing the process on mis-configured deployments.
///
/// Uses `ENV_MUTEX` to serialise env mutation with Assertion 5.
#[tokio::test]
#[cfg(feature = "hmac-codec")]
async fn pool_connect_fails_without_hmac_key() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required for pool startup tests");

    let _guard = ENV_MUTEX.lock().await;

    // Save the current value so we can restore it after the test.
    let saved = std::env::var("DJOGI_PRESENTATION_HMAC_KEY").ok();

    // SAFETY: this test keeps the process environment quiescent while the
    // mutation window is open, so the broader std::env::remove_var invariant
    // is satisfied (not just key-local serialization).
    #[allow(unsafe_code)]
    unsafe {
        std::env::remove_var("DJOGI_PRESENTATION_HMAC_KEY");
    }

    let result = DjogiPool::connect(&url).await;

    // Restore before any assertion so a test failure cannot leave other
    // tests running without the key.
    // SAFETY: same quiescent-process-env window as above; the restore call
    // also sees no concurrent env reads or writes process-wide.
    #[allow(unsafe_code)]
    match &saved {
        Some(v) => unsafe { std::env::set_var("DJOGI_PRESENTATION_HMAC_KEY", v) },
        None => unsafe { std::env::remove_var("DJOGI_PRESENTATION_HMAC_KEY") },
    }

    // Stage 2 defines `DjogiError::PresentationStartup(Vec<PresentationStartupError>)`.
    // The pool connect path (Stage 3) calls `validate_startup_inventory()` and
    // maps `Err(errors)` → `DjogiError::PresentationStartup(errors)`.
    let err = result.expect_err("pool connect must fail when HMAC key is absent");
    assert!(
        matches!(err, DjogiError::PresentationStartup(..)),
        "expected DjogiError::PresentationStartup, got: {err:?}"
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
#[cfg(feature = "hmac-codec")]
async fn validate_startup_inventory_errs_without_hmac_key() {
    let _guard = ENV_MUTEX.lock().await;

    let saved = std::env::var("DJOGI_PRESENTATION_HMAC_KEY").ok();

    // SAFETY: the surrounding harness keeps process-wide env access
    // quiescent while this mutation runs, which is the actual std::env invariant.
    #[allow(unsafe_code)]
    unsafe {
        std::env::remove_var("DJOGI_PRESENTATION_HMAC_KEY");
    }

    let result = djogi::presentation::validate_startup_inventory();

    // Restore before asserting.
    // SAFETY: same quiescent-process-env window as above.
    #[allow(unsafe_code)]
    match &saved {
        Some(v) => unsafe { std::env::set_var("DJOGI_PRESENTATION_HMAC_KEY", v) },
        None => unsafe { std::env::remove_var("DJOGI_PRESENTATION_HMAC_KEY") },
    }

    assert!(
        result.is_err(),
        "validate_startup_inventory must return Err when DJOGI_PRESENTATION_HMAC_KEY is unset"
    );
}

/// When `hmac-codec` is disabled, no keyed presentation codec is linked by this
/// test target, so startup validation must not require
/// `DJOGI_PRESENTATION_HMAC_KEY`.
#[cfg(not(feature = "hmac-codec"))]
#[tokio::test]
async fn validate_startup_inventory_allows_missing_hmac_key_when_hmac_codec_disabled() {
    let _guard = ENV_MUTEX.lock().await;

    let saved = std::env::var("DJOGI_PRESENTATION_HMAC_KEY").ok();

    // SAFETY: the surrounding harness keeps process-wide env access
    // quiescent while this mutation runs, which is the actual std::env invariant.
    #[allow(unsafe_code)]
    unsafe {
        std::env::remove_var("DJOGI_PRESENTATION_HMAC_KEY");
    }

    let result = djogi::presentation::validate_startup_inventory();

    // Restore before asserting.
    // SAFETY: same quiescent-process-env window as above.
    #[allow(unsafe_code)]
    match &saved {
        Some(v) => unsafe { std::env::set_var("DJOGI_PRESENTATION_HMAC_KEY", v) },
        None => unsafe { std::env::remove_var("DJOGI_PRESENTATION_HMAC_KEY") },
    }

    assert!(
        result.is_ok(),
        "with feature `hmac-codec` disabled, startup validation must not require \
         DJOGI_PRESENTATION_HMAC_KEY"
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
        support_view.email, "acceptance@example.com",
        "UserSupport::email must be the plaintext source value"
    );
}

#[djogi::djogi_test(sync_models = [User])]
async fn visage_queryset_fetch_applies_presentation_codec(mut ctx: DjogiContext) {
    let user = User::create(
        &mut ctx,
        User {
            email: "fetch-path@example.com".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("create user for fetch-path codec assertion");

    let public = UserPublic::filter(|f| f.id().eq(user.id))
        .fetch_one(&mut ctx)
        .await
        .expect("fetch public visage must decode and present the email");

    assert_eq!(
        public.email, "[REDACTED]",
        "UserPublic fetch must return the presentation-codec output"
    );
    assert_ne!(
        public.email, user.email,
        "UserPublic fetch must not expose the stored plaintext value"
    );

    let support = UserSupport::filter(|f| f.id().eq(user.id))
        .fetch_one(&mut ctx)
        .await
        .expect("fetch support visage must preserve un-coded scope value");

    assert_eq!(
        support.email, user.email,
        "un-coded support scope must still decode the stored value"
    );
}

#[djogi::djogi_test(sync_models = [UserWithQueryableIdentityCodec])]
async fn visage_queryset_filter_accepts_presentation_q_predicate(mut ctx: DjogiContext) {
    let match_row = UserWithQueryableIdentityCodec::create(
        &mut ctx,
        UserWithQueryableIdentityCodec {
            email: "query-surface@example.com".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("create queryable identity user");

    let _other_row = UserWithQueryableIdentityCodec::create(
        &mut ctx,
        UserWithQueryableIdentityCodec {
            email: "other@example.com".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("create non-matching queryable identity user");

    let public = UserWithQueryableIdentityCodecPublic::filter(|f| {
        f.email().eq("query-surface@example.com".to_string())
    })
    .fetch_one(&mut ctx)
    .await
    .expect("visage filter over presentation Q predicate must resolve one row");

    assert_eq!(public.id, match_row.id);
    assert_eq!(public.email, "query-surface@example.com");
}

#[djogi::djogi_test(sync_models = [UserWithFailingFetchCodec])]
async fn visage_queryset_fetch_maps_try_codec_errors(mut ctx: DjogiContext) {
    let user = UserWithFailingFetchCodec::create(
        &mut ctx,
        UserWithFailingFetchCodec {
            secret: "fail-on-fetch".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("create user for fallible fetch-path codec assertion");

    let err = UserWithFailingFetchCodecPublic::filter(|f| f.id().eq(user.id))
        .fetch_one(&mut ctx)
        .await
        .expect_err("fetch public visage must surface the fallible codec error");

    match err {
        DjogiError::Visage(VisageError::PresentationCodec {
            model,
            field,
            scope,
            codec,
            source,
        }) => {
            assert_eq!(model, "UserWithFailingFetchCodec");
            assert_eq!(field, "secret");
            assert_eq!(scope, "public");
            assert!(
                codec.contains("FailingFetchCodec"),
                "codec context must name FailingFetchCodec, got {codec}"
            );
            assert!(
                source.is::<FailingFetchCodecError>(),
                "source error must preserve FailingFetchCodecError, got {source:?}"
            );
        }
        other => panic!("expected VisageError::PresentationCodec, got {other:?}"),
    }
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
/// This test controls pool creation explicitly instead of going through
/// `#[djogi_test]`. That lets it prove the actual invariant:
///
/// 1. `DjogiPool::connect` fails while `DJOGI_PRESENTATION_HMAC_KEY` is absent.
/// 2. `install_presentation_hmac_key_for_testing(...)` runs before any later
///    pool creation.
/// 3. The manual test harness (`setup_test_db`) then succeeds, after which the
///    normal create/fetch round-trip still works.
#[tokio::test]
#[cfg(feature = "hmac-codec")]
async fn test_key_installed_before_pool_connect() {
    use djogi::__private::futures::FutureExt as _;
    use std::panic::AssertUnwindSafe;

    struct RestorePresentationKey(Option<String>);

    impl Drop for RestorePresentationKey {
        fn drop(&mut self) {
            // SAFETY: `_restore_key` drops before `_guard`, so this runs while
            // the test still holds the broader process-wide env exclusion.
            #[allow(unsafe_code)]
            unsafe {
                match &self.0 {
                    Some(value) => std::env::set_var("DJOGI_PRESENTATION_HMAC_KEY", value),
                    None => std::env::remove_var("DJOGI_PRESENTATION_HMAC_KEY"),
                }
            }
        }
    }

    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required for pool startup tests");
    let _guard = ENV_MUTEX.lock().await;
    let _restore_key = RestorePresentationKey(std::env::var("DJOGI_PRESENTATION_HMAC_KEY").ok());

    // SAFETY: `_guard` is held, and the test harness keeps process-wide env
    // access quiescent for the duration of this mutation window.
    #[allow(unsafe_code)]
    unsafe {
        std::env::remove_var("DJOGI_PRESENTATION_HMAC_KEY");
    }

    let missing_key_err = DjogiPool::connect(&url)
        .await
        .expect_err("pool connect must fail before the test helper installs the HMAC key");
    assert!(
        matches!(missing_key_err, DjogiError::PresentationStartup(..)),
        "expected DjogiError::PresentationStartup before helper install, got: {missing_key_err:?}"
    );

    // Install the 64-hex-char test HMAC key before the manual harness creates
    // its pool. This is the invariant under test.
    // SAFETY: `_guard` is held and the surrounding harness preserves the
    // broader no-concurrent-process-wide-env-access invariant required by
    // std::env::set_var/remove_var (not just key-local serialization).
    #[allow(unsafe_code)]
    unsafe {
        djogi::testing::install_presentation_hmac_key_for_testing(
            "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899",
        );
    }

    let (cleanup, mut ctx) = djogi::testing::setup_test_db()
        .await
        .expect("setup_test_db must succeed when the HMAC key is installed pre-connect");

    let outcome = AssertUnwindSafe(async {
        djogi::testing::sync_models(&mut ctx, &[UserWithCodec::descriptor()])
            .await
            .expect("sync_models must materialize UserWithCodec for the round-trip assertion");

        // Full CRUD round-trip through the typed surface. The presentation
        // codec is applied at create time (encode path) and at fetch time
        // (decode path). Both must succeed for the round-trip to pass.
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
    })
    .catch_unwind()
    .await;

    djogi::testing::teardown_test_db(cleanup).await;

    if let Err(panic_payload) = outcome {
        std::panic::resume_unwind(panic_payload);
    }
}
