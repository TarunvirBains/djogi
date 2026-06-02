// `#[model(hooks)]` opts a model into hook dispatch.
//
// When the attribute is present AND the adopter implements `ModelHooks`,
// the macro emits both halves of the seal:
//
//   impl ::djogi::__private::hooks::Sealed for User {}
//   impl ::djogi::__private::hooks::HasHooks for User {}
//
// We assert at compile time that `User: HasHooks` by passing it through
// `requires<T: HasHooks>()`. If either impl is missing, the bound check
// fails with E0277 — exactly the diagnostic an adopter would see on a
// model that forgot `impl ModelHooks for User`.

use djogi::hooks::HasHooks;
use djogi::prelude::*;
use djogi::{DjogiContext, DjogiError};

#[model(table = "phase8_hooks_users", hooks)]
#[derive(Debug, Clone)]
pub struct User {
    pub email: String,
}

// Adopter's own `ModelHooks` impl. Emitting both `#[model(hooks)]` and
// this `impl` is the canonical opt-in shape. The macro emits the
// `HasHooks` impl based on the attribute alone — the type system
// rejects a model that opted in but forgot the sibling impl, because
// `HasHooks: ModelHooks`.
impl ModelHooks for User {
    async fn before_create(&mut self, _ctx: &mut DjogiContext) -> Result<(), DjogiError> {
        // Body irrelevant for this fixture; the point is that the
        // `HasHooks` impl exists on `User`.
        Ok(())
    }
}

// Compile-time witness — only models that satisfy the sealed
// `HasHooks` bound reach this call. Any breakage in the macro-emitted
// pair turns this into an E0277 at the call site below.
fn requires<T: HasHooks>() {}

fn main() {
    requires::<User>();
}
