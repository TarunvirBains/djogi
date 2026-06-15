//! `IntoConflictUpdates` is a sealed bridge trait — external crates must not
//! be able to implement it. This fixture pins that the supertrait seal
//! rejects a downstream impl.
use djogi::prelude::*;

struct Rogue;

impl<S: djogi::prelude::Model, T: djogi::prelude::Model> IntoConflictUpdates<S, T> for Rogue {
    fn into_conflict_updates(self) -> Vec<ConflictUpdate<S, T>> {
        Vec::new()
    }
}

fn main() {}
