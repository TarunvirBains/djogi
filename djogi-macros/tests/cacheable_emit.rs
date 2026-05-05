//! Cluster 8δ T7.2 — runtime checks for the auto-emitted
//! `impl Cacheable for {Model}` and `impl DeltaSyncCacheable for {Model}`.
//!
//! Ships in `djogi-macros/tests/` rather than `djogi/tests/` because
//! the surface under test is what the macro emits — `#[derive(Model)]`
//! is `djogi-macros`-owned, the trait re-exports are `djogi`-owned,
//! and putting the integration test alongside the macro keeps the
//! provenance clear. The trybuild compile-pass fixtures
//! (`tests/compile_pass/phase8_t7_cacheable_*.rs`) cover the
//! macro-emission side from a standalone-fixture angle; this file is
//! the in-crate side — uses `#[derive(Model)]` directly through the
//! `djogi` dev-dep prelude and asserts on the resulting trait
//! contract.
//!
//! Spec anchor:
//!   docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md
//!   §3 commit T7.2 — "Test names + assertions" bullet.

use djogi::prelude::*;

// Bring the re-exported `Cacheable` / `DeltaSyncCacheable` traits into
// scope for method dispatch. `djogi::types::Cacheable` is the macro-
// routing path the auto-emit targets; `djogi::cache::Cacheable`
// (re-exported from the same sassi trait via `djogi/src/cache.rs`)
// resolves to the same trait, so importing either is equivalent.
// We import via `djogi::types` to mirror the macro-emission target
// path exactly.
use djogi::types::{Cacheable, DeltaSyncCacheable};

#[model(table = "phase8_t7_cacheable_emit_default")]
#[derive(Debug, Clone)]
pub struct DefaultModel {
    pub label: String,
}

#[model(
    table = "phase8_t7_cacheable_emit_watermark",
    watermark_field = "expires_at",
    no_default
)]
#[derive(Debug, Clone)]
pub struct WatermarkModel {
    pub label: String,
    pub expires_at: ::djogi::types::DateTime,
}

/// `Cacheable::Id` resolves to the framework-injected `HeerIdDesc`
/// (the post-Phase-7-Zero-2 default — recency-biased ascending HeerId).
///
/// Compile-time check: a function generic over `T: Cacheable` whose
/// body bounds `T::Id` to the expected concrete type pins the emitted
/// associated type. If the macro emitted the wrong type, this fixture
/// would fail at monomorphisation time with a "type mismatch" error.
#[test]
fn cacheable_emitted_for_default_model() {
    fn assert_id_type<T: Cacheable<Id = ::djogi::types::HeerIdDesc>>() {}
    assert_id_type::<DefaultModel>();
}

/// `Cacheable::id(&self)` clones the `id` field. The runtime check
/// confirms the emitted body is `self.id.clone()` — a model with
/// `id` zero-valued (the default-impl sentinel) round-trips through
/// `Cacheable::id` to exactly the same value.
#[test]
fn cacheable_id_returns_self_id_field() {
    let m = DefaultModel::default();
    let cached_id = <DefaultModel as Cacheable>::id(&m);
    // `HeerIdDesc::sentinel()` is the default-impl zero value per
    // `inject::generate_default_impl`; the auto-emitted `id()` must
    // return the same value through the trait dispatch.
    assert_eq!(cached_id, m.id);
}

/// `DeltaSyncCacheable::Watermark` resolves to the type of the field
/// named by `#[model(watermark_field = "expires_at")]` — `DateTime`
/// in this fixture. Without the override, the watermark defaults to
/// `updated_at: DateTime` (always present per Phase 7 framework-field
/// injection).
#[test]
fn delta_sync_cacheable_watermark_uses_named_field() {
    fn assert_watermark_type<T: DeltaSyncCacheable<Watermark = ::djogi::types::DateTime>>() {}
    assert_watermark_type::<WatermarkModel>();

    // The default-watermark branch — `DefaultModel` has no
    // `watermark_field` override, so the macro falls back to
    // `updated_at`, which is also `DateTime`. Same `Watermark` type
    // resolves through both paths.
    assert_watermark_type::<DefaultModel>();
}

/// `DeltaSyncCacheable::watermark(&self)` clones the named field.
/// For `WatermarkModel`, that's `expires_at`; for `DefaultModel`,
/// it's `updated_at`. Both flow through the same trait dispatch.
///
/// `WatermarkModel` carries `#[model(no_default)]` because
/// `time::OffsetDateTime` does not implement `Default` — every
/// field must be initialised explicitly. We use `OffsetDateTime::UNIX_EPOCH`
/// for both timestamp slots since the test only exercises trait
/// dispatch, not real-DB roundtrip.
#[test]
fn delta_sync_cacheable_watermark_returns_field_value() {
    let epoch = ::djogi::types::DateTime::UNIX_EPOCH;
    let m = WatermarkModel {
        id: <::djogi::types::HeerIdDesc as ::djogi::primary_key::PrimaryKey>::sentinel(),
        created_at: epoch,
        updated_at: epoch,
        label: "test".to_string(),
        expires_at: epoch,
    };
    let watermark = <WatermarkModel as DeltaSyncCacheable>::watermark(&m);
    assert_eq!(watermark, m.expires_at);

    let d = DefaultModel::default();
    let default_watermark = <DefaultModel as DeltaSyncCacheable>::watermark(&d);
    assert_eq!(default_watermark, d.updated_at);
}
