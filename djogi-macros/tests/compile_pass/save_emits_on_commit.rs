// Macro-emitted `save` / `delete` bodies reference
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
// See also: `save_emits_on_commit_ranjid.rs` for the `pk = RanjId` variant.

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
