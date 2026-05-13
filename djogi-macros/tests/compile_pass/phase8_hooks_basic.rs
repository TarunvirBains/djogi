// Phase 8α T1.7 — Minimal compile-pass fixture: a model annotated with
// `#[model(hooks)]` plus a hand-written `impl ModelHooks for M` that
// overrides one method body. This is the canonical adopter shape and
// the smallest fixture that proves the hook opt-in path keeps compiling
// alongside the broader lihaaf matrix.
//
// Distinct from `phase8_hooks_attribute.rs` (T1.3): that fixture
// witnesses the macro-emitted `HasHooks` bound through a generic
// `requires<T: HasHooks>()` call. This fixture exercises the more
// realistic adopter shape — declare the model, override one hook with
// a non-empty body, and confirm the whole stack compiles.
//
// Every lihaaf compile-fixture must have
// `fn main() {}` so the stored `.stderr` (when compile-fail) does not
// pick up `E0601 (main not found)` noise. compile-pass fixtures need
// `fn main()` for the same reason — the binary still has to link.

use djogi::prelude::*;
use djogi::{DjogiContext, DjogiError};

#[model(table = "phase8_hooks_basic_widgets", hooks)]
#[derive(Debug, Clone)]
pub struct Widget {
    pub name: String,
    pub count: i32,
}

impl ModelHooks for Widget {
    async fn before_create(
        &mut self,
        _ctx: &mut DjogiContext,
    ) -> Result<(), DjogiError> {
        // Trivial mutation — proves the hook receives `&mut self` and
        // can in fact mutate the in-memory model before the INSERT
        // composes its `RETURNING` clause. The body intentionally does
        // no I/O so the fixture stays a pure compile-pass.
        if self.count < 0 {
            self.count = 0;
        }
        Ok(())
    }
}

fn main() {}
