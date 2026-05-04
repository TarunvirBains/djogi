// Phase 8α T1.7 — Compile-fail fixture: a model declared with
// `#[model(hooks)]` but with NO `impl ModelHooks for M`.
//
// The macro emits both halves of the seal:
//
//     impl ::djogi::__private::hooks::Sealed for Widget {}
//     impl ::djogi::__private::hooks::HasHooks for Widget {}
//
// The `HasHooks` trait has `ModelHooks` as a supertrait. Without a
// hand-written `impl ModelHooks for Widget`, the `HasHooks` impl fails
// to typecheck because Widget does not satisfy its supertrait bound,
// surfacing an implementer-actionable "the trait `ModelHooks` is not
// implemented for `Widget`" diagnostic — the message names the trait
// the adopter must hand-roll, exactly the error class the spec calls
// for.
//
// Per `feedback_trybuild_fixtures.md`, `fn main() {}` is mandatory.

use djogi::prelude::*;

#[model(table = "phase8_hooks_no_impl_widgets", hooks)]
#[derive(Debug, Clone)]
pub struct Widget {
    pub name: String,
}

fn main() {}
