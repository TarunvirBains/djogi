// Cluster 8δ T7.6 — `DjogiContext::use_punnu` compile-pass fixture.
//
// Witnesses that `ctx.use_punnu(&p)` resolves to the expected signature:
//
//   fn(&DjogiContext, &Arc<Punnu<T>>) -> Arc<Punnu<T>>
//
// This is a non-emitted, non-macro method on `DjogiContext`; the fixture
// lives here (alongside other T7 compile_pass checks) rather than in
// `djogi/tests/` because the lihaaf harness owns this bucket and the
// type-witness pattern is the same as T7.4/T7.5.
//
// T7.6 is non-emitted code — `use_punnu` is a method on `DjogiContext`,
// not a macro expansion. The path-routing rule (`feedback_macro_path_routing.md`)
// therefore does NOT apply here (path routing governs macro-emitted code only).
// The method spells `sassi::Punnu` directly in `djogi/src/context.rs`; adopters
// reach it through the re-exported `djogi::cache::Punnu` path, which is what
// this fixture uses.
//
// Every lihaaf compile-fixture must have
// `fn main` so the stored binary can link.
//
// Spec anchor:
//   docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md
//   §3 commit T7.6 — compile-fixture bullet (the plan calls it
//   "Trybuild fixture"; this fixture is now run through lihaaf).

use djogi::prelude::*;

// Minimal fixture model — must derive Cacheable (i.e. have a pk).
#[model(table = "phase8_t7_6_compile_pass_rows")]
#[derive(Debug, Clone)]
pub struct Phase8T76CompilePassRow {
    pub value: String,
}

fn _accept_use_punnu<T: ::djogi::types::Cacheable + 'static>() {
    fn _signature_check<U: ::djogi::types::Cacheable + 'static>(
        ctx: &::djogi::DjogiContext,
        p: &::std::sync::Arc<::djogi::cache::Punnu<U>>,
    ) -> ::std::sync::Arc<::djogi::cache::Punnu<U>> {
        ctx.use_punnu(p)
    }
    let _: fn(
        &::djogi::DjogiContext,
        &::std::sync::Arc<::djogi::cache::Punnu<T>>,
    ) -> ::std::sync::Arc<::djogi::cache::Punnu<T>> = _signature_check::<T>;
}

fn main() {
    _accept_use_punnu::<Phase8T76CompilePassRow>();
}
