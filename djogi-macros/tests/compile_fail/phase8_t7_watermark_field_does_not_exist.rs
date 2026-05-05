// Cluster 8δ T7.2 — `#[model(watermark_field = "missing")]` rejects with
// a span-precise compile error pointing at the literal string in the
// attribute.
//
// The Cacheable auto-emit pass walks the post-injection field list and
// confirms the named watermark field exists; a missing field surfaces
// as a clean diagnostic at the attribute literal rather than as a
// downstream "no field named …" error inside the emitted impl body.
//
// Per `feedback_trybuild_fixtures.md`, every compile-fail fixture must
// have `fn main` so the stored `.stderr` does not pick up E0601
// noise.
//
// Spec anchor:
//   docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md
//   §3 commit T7.2 — "Compile-fail fixtures" bullet.

use djogi::prelude::*;

#[model(
    table = "phase8_t7_watermark_missing_rows",
    watermark_field = "no_such_field",
)]
#[derive(Debug, Clone)]
pub struct WatermarkMissingRow {
    pub label: String,
}

fn main() {}
