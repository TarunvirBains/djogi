// A model declared WITHOUT `#[model(hooks)]` compiles
// fine and gets NO `HasHooks` impl. This is the zero-overhead default:
// the CRUD terminals (T1.4–T1.6) read the `HasHooks` bound at
// monomorphisation time, so without the impl the dispatch helpers fold
// to no-ops that LLVM elides regardless of LTO settings (§D2).
//
// We deliberately do NOT call `requires<T: HasHooks>()` here — that
// would fail with E0277, which is the correct behaviour for the opt-out
// path. The fixture is a compile-pass purely on the model declaration:
// the macro must continue to accept ordinary models without injecting
// an unsolicited hook impl.

use djogi::prelude::*;

#[model(table = "phase8_no_hooks_widgets")]
#[derive(Debug, Clone)]
pub struct Widget {
    pub name: String,
}

fn main() {
    // The model compiles. No HasHooks bound is asserted; the absence of
    // the impl is the correctness condition this fixture exercises.
    let _w = Widget {
        name: "ok".into(),
        ..Default::default()
    };
}
