// Phase 7-Zero-2 T2 — unknown `pk = X` identifiers must be rejected with a
// diagnostic that enumerates the accepted set. Task 3 adds
// `PkStrategy::Custom` + adopter-declared PK types; until then, every
// unknown single-segment identifier is an error (not a fall-through to a
// custom variant that hasn't been wired yet).
use djogi::prelude::*;

#[model(table = "foo", pk = NotAPkStrategy)]
pub struct Foo {
    pub bar: String,
}

fn main() {}
