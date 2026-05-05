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
// **Snapshot maintenance.** trybuild 1.0 has no wildcard-or-placeholder
// notation in `.stderr` files (only path / version / temp-dir
// normalisation), so the stored snapshot pins the file:line:col block
// + the source-line excerpt verbatim. If this file is edited and the
// `watermark_field = "no_such_field"` literal moves off line 35, the
// snapshot drifts and the test fails with `Mismatch`. Regenerate with
// `TRYBUILD=overwrite cargo test -p djogi-macros --test trybuild_tests
// compile_fail_phase8_t7 -- --test-threads=1`. The fixture decouples
// from neighbouring-file edits because trybuild compiles each `.rs`
// as a standalone crate; only edits inside *this* file shift the
// recorded line number.
//
// Spec anchor:
//   docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md
//   §3 commit T7.2 — "Compile-fail fixtures" bullet, plus the T7.2
//   phase amendment block (Codex Finding 6 stderr-drift discussion).

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
