// Cluster 8δ T7.4 — `#[derive(Model)]` auto-emits a `SassiBootHook`
// `inventory::submit!` block.
//
// Pins the spec contract that a bare `#[model(...)]` declaration emits
// the boot hook needed for `DjogiContext::from_pool` to register a
// `Punnu<T>` automatically. The fixture is compile-only — `_use_boot_hook_type`
// witnesses that `::djogi::SassiBootHook` is reachable from the crate root,
// and `_accept_cacheable::<BootHookRow>` witnesses that the model gets a
// `Cacheable` impl through `::djogi::types`. Macro path-routing
// (`feedback_macro_path_routing.md`) is enforced structurally, not by
// this fixture: djogi's `cache/mod.rs` and `lib.rs` re-export the sassi
// types under `::djogi::*` paths, so a hypothetical `::sassi::*` typo
// in `cacheable.rs`'s emission would fail to compile in any adopter
// crate that has only `djogi` in its `Cargo.toml`. (It would NOT fail
// here — `djogi-macros/Cargo.toml` carries `sassi` as a dev-dep for
// an unrelated 8γ fixture, so trybuild has `sassi` reachable through
// the test-binary dep graph. The structural guarantee is the actual
// contract.)
//
// Per `feedback_trybuild_fixtures.md`, every trybuild fixture must
// have `fn main` so the stored binary can link.
//
// Spec anchor:
//   docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md
//   §3 commit T7.4 — "Trybuild fixture" bullet.

use djogi::prelude::*;

#[model(table = "phase8_t7_4_boot_hook_rows")]
#[derive(Debug, Clone)]
pub struct BootHookRow {
    pub name: String,
}

// Verify the type is Cacheable (boot hook registration requires this).
fn _accept_cacheable<T: ::djogi::types::Cacheable + 'static>() {}

// Verify SassiBootHook is reachable from the crate root.
fn _use_boot_hook_type() -> Option<::djogi::SassiBootHook> {
    None
}

fn main() {
    _accept_cacheable::<BootHookRow>();
    let _ = _use_boot_hook_type();
}
