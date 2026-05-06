// Cluster 8δ T7.2 — `#[model(watermark_field = "...")]` overrides the
// default `updated_at` watermark for the auto-emitted
// `DeltaSyncCacheable` impl.
//
// Pins the spec contract that an adopter who needs delta-sync to
// pivot off a non-`updated_at` field — `expires_at`, a domain-
// specific `recorded_at`, a monotonic `version: i64`, etc. —
// declares the override in `#[model(...)]` and the emitted
// `DeltaSyncCacheable::Watermark` resolves to the named field's
// declared type without further derive attributes.
//
// `#[model(no_default)]` is required because `time::OffsetDateTime`
// does not implement `Default` — every field must be initialised
// explicitly when the user model carries a non-Default field type.
//
// Per `feedback_trybuild_fixtures.md`, every trybuild fixture must
// have `fn main` so the stored binary can link.
//
// Spec anchor:
//   docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md
//   §3 commit T7.2 — "Test names + assertions" bullet.

use djogi::prelude::*;

#[model(
    table = "phase8_t7_cacheable_with_watermark_rows",
    watermark_field = "expires_at",
    no_default,
)]
#[derive(Debug, Clone)]
pub struct WatermarkedRow {
    pub label: String,
    pub expires_at: ::djogi::types::DateTime,
}

// `DeltaSyncCacheable<Watermark = DateTime>` reachable on
// `WatermarkedRow` proves the macro emitted the impl pointing at
// `expires_at` (whose declared type is `DateTime`). The
// `Watermark = ...` constraint binds at impl-resolution time, so a
// macro that emitted the wrong field's type would fail this check
// at monomorphisation.
fn _accept_delta_sync<
    T: ::djogi::types::DeltaSyncCacheable<Watermark = ::djogi::types::DateTime>,
>() {
}

fn main() {
    _accept_delta_sync::<WatermarkedRow>();
}
