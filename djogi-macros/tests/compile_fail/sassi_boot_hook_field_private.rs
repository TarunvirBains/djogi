// v0.1.0 doc surface (#125) — `SassiBootHook`'s tuple field and
// tuple-struct constructor are gated against adopter code.
//
// `SassiBootHook` is link-time machinery emitted by `#[model]`.
// The struct stays `pub` (and `#[doc(hidden)]`) so macro-emitted code
// in adopter crates can name `::djogi::SassiBootHook` without an
// adopter-side `sassi` dep, but the tuple field and the implicit tuple
// constructor are gated to the framework's own crate. Adopter code:
//
// - cannot read `hook.0` (the inner `fn(&mut sassi::Sassi)` pointer),
// - cannot call the tuple-struct constructor `SassiBootHook(some_fn)`.
//
// Both reaches are field-level privacy violations (E0451 / E0616), not
// type-level resolution failures — the type itself is still nameable
// from adopter code (`Option<SassiBootHook>` works), but its inner
// shape is no longer exposed.
//
// Macro-emitted code reaches the constructor through the hidden public
// `SassiBootHook::__djogi_from_model_macro(...)` associated function
// instead, so this gate does not break `#[model]` expansion (verified
// separately by the `t7_4_punnu_boot_hook_emitted` and
// `adopter_crate_isolation` buckets).
//
// Per the lihaaf compile-fixture contract, every compile-fail fixture must
// have `fn main` so the stored `.stderr` does not pick up E0601
// noise.
//
// **Snapshot maintenance.** lihaaf 1.0 has no wildcard-or-placeholder
// notation in `.stderr` files (only path / version / temp-dir
// normalisation), so the stored snapshot pins the file:line:col block
// + the source-line excerpt verbatim. If this file is edited and the
// `SassiBootHook(...)` / `hook.0` lines move, the snapshot drifts and
// the lihaaf gate fails with `SNAPSHOT_DIFF`. Regenerate with
// `cargo lihaaf --manifest-path djogi-macros/Cargo.toml \
//  --filter phase8_5_c2_125 --bless -j 4`.
//
// Spec anchor: GH #125 "v0.1.0 doc surface: narrow SassiBootHook public field".

use djogi::SassiBootHook;

fn registration_fn(_sassi: &mut djogi::cache::Sassi) {}

fn read_inner(hook: &SassiBootHook) -> fn(&mut djogi::cache::Sassi) {
 // Field `0` is `pub(crate)` — adopter crates cannot read it.
 hook.0
}

fn build_hook() -> SassiBootHook {
 // Tuple-struct constructor is gated by the field privacy — adopter
 // crates cannot call `SassiBootHook(fn_ptr)` directly. Macro-emitted
 // code routes through `SassiBootHook::__djogi_from_model_macro(fn_ptr)`
 // instead.
 SassiBootHook(registration_fn)
}

fn main() {
 let hook = build_hook();
 let _f = read_inner(&hook);
}
