// Verifies that `#[model(table = "...", through)]` parses, compiles, and
// populates `ModelDescriptor::is_through = true`. The through-marker flag
// is a Task 6 foundation for the upcoming `ManyToMany<Target>` trait: the
// runtime side gets the marker now so later commits can wire it into the
// trait and tooling without a second macro churn.
//
// Pinned invariants:
//
//   - `through` is a bare flag (mirrors `no_default`), not a `key = "value"`
//     form. Duplicate/unknown-key errors are covered by the attrs parser's
//     existing tests.
//   - `ModelDescriptor::is_through` is `true` on the through model and
//     `false` on ordinary models — this fixture asserts both branches.
//
// The two FK columns on `PersonGroup` mirror the `person_groups_p3` DDL
// in `tests/integration/migrations/relations/007_person_groups.sql`, so this
// file also catches a type-shape regression if either side of that pair
// drifts.

use djogi::prelude::*;
use djogi::relation::ForeignKey;

#[model(table = "persons")]
#[derive(Debug, Clone)]
pub struct Person {
    pub name: String,
}

#[model(table = "groups")]
#[derive(Debug, Clone)]
pub struct Group {
    pub name: String,
}

#[model(table = "person_groups", through, no_default)]
#[derive(Debug, Clone)]
pub struct PersonGroup {
    pub person_id: ForeignKey<Person>,
    pub group_id: ForeignKey<Group>,
    pub role: String,
}

fn main() {
    // Standalone models report `is_through == false`.
    assert!(!<Person as Model>::descriptor().is_through);
    assert!(!<Group as Model>::descriptor().is_through);

    // Through model reports `is_through == true`.
    assert!(<PersonGroup as Model>::descriptor().is_through);
}
