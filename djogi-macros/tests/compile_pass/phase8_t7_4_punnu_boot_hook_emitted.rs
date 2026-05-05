// Cluster 8δ T7.4 — `#[derive(Model)]` auto-emits a `SassiBootHook`
// `inventory::submit!` block.
//
// Pins the spec contract that a bare `#[model(...)]` declaration emits
// the boot hook needed for `DjogiContext::from_pool` to register a
// `Punnu<T>` automatically. The fixture is compile-only: if the
// macro-emitted `inventory::submit!` block spells any path through
// `::sassi::*` or `::inventory::*` directly (violating
// `feedback_macro_path_routing.md`), the fixture fails to compile
// because the test-crate only has `djogi` in its dependency graph —
// not `sassi` or `inventory` directly.
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
