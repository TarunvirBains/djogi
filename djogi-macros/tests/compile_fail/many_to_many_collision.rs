// Two `many_to_many!` invocations producing the same inherent-method
// name (`groups`) on the same source type (`Person`) must fail to
// compile. The macro does not (yet) perform a cross-invocation
// collision check via inventory — that lands in a follow-up phase —
// but each invocation emits a plain `impl Person { pub fn groups(...) }`
// block, so rustc's duplicate-definition error fires on the second
// invocation and turns the mistake into a compile-time failure.
//
// This pins the collision story for `many_to_many!` symmetric with
// `reverse_relation_duplicate_accessor.rs`: same-receiver /
// same-method emissions collide at the inherent-method layer
// regardless of which macro (reverse-FK, reverse-O2O, M2M) produced
// them. A future refactor that shifts the emission into an extension
// trait (which would silently lose the collision check) trips this
// fixture instead of slipping by.

use djogi::prelude::*;
use djogi::relation::ForeignKey;

#[model(table = "persons_mmc")]
#[derive(Debug, Clone)]
pub struct Person {
    pub name: String,
}

#[model(table = "groups_mmc")]
#[derive(Debug, Clone)]
pub struct Group {
    pub name: String,
}

#[model(table = "person_groups_mmc", through, no_default)]
#[derive(Debug, Clone)]
pub struct PersonGroup {
    pub person_id: ForeignKey<Person>,
    pub group_id: ForeignKey<Group>,
}

// First declaration — legitimate.
djogi::many_to_many!(
    Person, Group,
    through = PersonGroup,
    this_fk = person_id,
    that_fk = group_id,
    relation = "groups"
);

// Second declaration with the same `relation` on the same source
// type. The emitted `impl Person { pub fn groups(...) }` collides
// with the first at rustc's duplicate-definition check, failing the
// build here. The trait impl `impl ManyToMany<Group> for Person` also
// duplicates, compounding the failure.
djogi::many_to_many!(
    Person, Group,
    through = PersonGroup,
    this_fk = person_id,
    that_fk = group_id,
    relation = "groups"
);

fn main() {}
