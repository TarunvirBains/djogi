// `#[derive(Model)]` auto-emits Cacheable.
//
// Pins the spec contract that a bare `#[model(...)]` declaration —
// no extra derive attributes, no hand-rolled `impl Cacheable` — is
// sufficient to make the type usable as `T: ::djogi::types::Cacheable`
// (and therefore as `Punnu<T>` per the T7.1 re-export surface).
//
// The fixture exercises the macro-emission path that the in-crate
// integration test in `djogi-macros/tests/cacheable_emit.rs` cannot
// — lihaaf compiles each fixture as a standalone rustc invocation, which
// catches every kind of "name routes through the wrong path"
// regression that an in-crate test (where every internal name
// resolves directly) would miss.
//
// Every lihaaf compile-fixture must
// have `fn main` so the stored binary can link.
//
// See also: `djogi-macros/tests/cacheable_emit.rs` for the in-crate assertions.

use djogi::prelude::*;

#[model(table = "phase8_t7_cacheable_default_rows")]
#[derive(Debug, Clone)]
pub struct DefaultRow {
 pub label: String,
}

// The load-bearing surface check: a function generic over
// `T: ::djogi::types::Cacheable` accepts `DefaultRow`. The bound is
// reachable through djogi's macro-routing path (`::djogi::types`),
// proving the auto-emit emits the impl through that path and not
// via a `::sassi::*` direct reference.
fn _accept_cacheable<T: ::djogi::types::Cacheable>() {}

// Concrete usability through the `Punnu<T>` surface — the auto-
// emitted impl must satisfy every bound `Punnu` requires of `T`.
fn _build_punnu() -> ::djogi::cache::Punnu<DefaultRow> {
 ::djogi::cache::Punnu::<DefaultRow>::builder().build()
}

fn main() {
 _accept_cacheable::<DefaultRow>();
 let _punnu = _build_punnu();
}
