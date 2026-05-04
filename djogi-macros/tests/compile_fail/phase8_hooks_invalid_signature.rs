// Phase 8α T1.7 — Compile-fail fixture: `impl ModelHooks` with the wrong
// receiver mutability on `before_create`. The trait declares
//
//     fn before_create(&mut self, ctx: &mut DjogiContext) -> impl Future<…>
//
// The override below uses `&self` instead of `&mut self`. Rust rejects
// the impl with a method-receiver mismatch error — exactly the
// diagnostic an adopter would see if they typed the signature wrong.
//
// Pinning the diagnostic via trybuild keeps the error message stable
// across rustc upgrades; if the message changes we want to know.
//
// Per `feedback_trybuild_fixtures.md`, `fn main() {}` is mandatory so
// the captured `.stderr` does NOT contain `E0601 (main not found)`
// noise alongside the load-bearing receiver-mismatch error.

use djogi::prelude::*;
use djogi::{DjogiContext, DjogiError};

#[model(table = "phase8_hooks_invalid_sig_widgets", hooks)]
#[derive(Debug, Clone)]
pub struct Widget {
    pub name: String,
}

impl ModelHooks for Widget {
    // Wrong receiver: trait says `&mut self`, override says `&self`.
    async fn before_create(
        &self,
        _ctx: &mut DjogiContext,
    ) -> Result<(), DjogiError> {
        Ok(())
    }
}

fn main() {}
