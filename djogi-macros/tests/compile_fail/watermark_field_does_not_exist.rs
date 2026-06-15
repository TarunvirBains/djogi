// `#[model(watermark_field = "missing")]` rejects with
// a span-precise compile error pointing at the literal string in the
// attribute.
//
// The Cacheable auto-emit pass walks the post-injection field list and
// confirms the named watermark field exists; a missing field surfaces
// as a clean diagnostic at the attribute literal rather than as a
// downstream "no field named …" error inside the emitted impl body.
//
// Per the lihaaf compile-fixture contract, every compile-fail fixture must
// have `fn main` so the stored `.stderr` does not pick up E0601
// noise.
//
// **Snapshot maintenance.** lihaaf 1.0 has no wildcard-or-placeholder
// notation in `.stderr` files (only path / version / temp-dir
// normalisation), so the stored snapshot pins the file:line:col block
// + the source-line excerpt verbatim. If this file is edited and the
// `watermark_field = "no_such_field"` literal moves off line 35, the
// snapshot drifts and the lihaaf gate fails with `SNAPSHOT_DIFF`.
// Regenerate with
// `cargo lihaaf --manifest-path djogi-macros/Cargo.toml \
//  --filter watermark_field_does_not_exist --bless -j 4`. The fixture decouples from
// neighbouring-file edits because lihaaf compiles each `.rs` as a
// standalone rustc invocation; only edits inside *this* file shift the
// recorded line number.
//
// See also: `cacheable_with_watermark_field.rs` (compile-pass) for the positive case.

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
