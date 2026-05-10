// Cluster 8δ T7.4 — `#[derive(Model)]` auto-emits a `SassiBootHook`
// `inventory::submit!` block.
//
// Pins the spec contract that a bare `#[model(...)]` declaration emits
// the boot hook needed for `DjogiContext::from_pool` to register a
// `Punnu<T>` automatically. The fixture is compile-only —
// `_use_boot_hook_type` witnesses that `::djogi::SassiBootHook` is
// reachable from the crate root, and `_accept_cacheable::<BootHookRow>`
// witnesses that the model gets a `Cacheable` impl through
// `::djogi::types`.
//
// # Scope — user-surface shape only, NOT path-routing isolation
//
// This trybuild fixture compiles in the same dep graph as
// `djogi-macros` (trybuild copies the test crate's dev-deps into the
// generated fixture crate). `djogi-macros/Cargo.toml` lists `sassi`,
// `serde`, and `serde_json` as `[dev-dependencies]` for unrelated
// fixtures (8γ T6.10 lookup-op no-regex lock, JsonbSchema serde
// derives), so a stray `::sassi::*` / `::serde::*` typo in macro
// emission would compile here too — this fixture cannot catch that
// regression. Its job is the user-surface shape: that adopters can
// reach `Cacheable`, `SassiBootHook`, and `Punnu` through `::djogi::*`
// names.
//
// The `feedback_macro_path_routing.md` invariant ("macro paths route
// through `::djogi::*` only — no direct `::sassi::*` / `::heeranjid::*` /
// `::time::*` etc.") is enforced by the sibling integration test
// `djogi-macros/tests/adopter_crate_isolation.rs`, which shells out
// `cargo check` against a standalone fixture crate whose
// `[dependencies]` table contains only `djogi` and whose own
// `[workspace]` block keeps cargo from absorbing it into djogi's
// outer workspace. That driver is the actual adopter-isolation guard;
// this fixture is the faster, focused user-facing surface check.
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
