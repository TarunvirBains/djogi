// Cluster 8δ T7.5 — macro-emitted `save` / `delete` bodies reference
// `::djogi::cache::InvalidationReason` only; no `::sassi::*` leakage.
//
// The fixture verifies that a bare `#[model]` declaration compiles
// cleanly when the macro injects the `on_commit` invalidation hook.
// If any path in the emitted hook block is wrong (`::sassi::*` instead
// of `::djogi::*`, wrong variant name, missing `InvalidationReason`
// re-export), this fixture fails to compile — catching the error before
// the integration suite runs.
//
// Path-routing contract (per `feedback_macro_path_routing.md`):
//   - `::djogi::cache::InvalidationReason::OnSave`  — save path
//   - `::djogi::cache::InvalidationReason::OnDelete` — delete path
//   - `ctx.punnu::<Self>()` — guarded by `if let Some(...)`
//   - `ctx.on_commit(...)` — called only when `punnu` is `Some`
//
// Every lihaaf compile-fixture must
// have `fn main` so the stored binary can link.
//
// Spec anchor:
//   docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md
//   §3 commit T7.5 — compile-pass fixture bullet (the plan calls it
//   "Trybuild compile-pass fixture"; this fixture is now run through
//   lihaaf).

use djogi::prelude::*;

#[model(table = "phase8_t7_5_on_commit_rows")]
#[derive(Debug, Clone)]
pub struct OnCommitRow {
    pub label: String,
}

// Witness that `InvalidationReason` is reachable through `djogi::cache`
// (the path the macro-emitted code spells).
fn _accept_invalidation_reason(
    _: ::djogi::cache::InvalidationReason,
) {
}

fn main() {
    _accept_invalidation_reason(::djogi::cache::InvalidationReason::OnSave);
    _accept_invalidation_reason(::djogi::cache::InvalidationReason::OnDelete);
}
