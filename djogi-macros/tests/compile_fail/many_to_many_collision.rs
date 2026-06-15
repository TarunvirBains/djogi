// Two `many_to_many!` invocations producing the same `relation` name
// (`groups`) on the same source type (`Person`) must fail to compile.
// The macro emits a per-relation trait
// `{Source}{Relation-pascal}ManyToManyRelation` plus its impl (GH issue
// #39 — coherence-rule rationale, see `djogi-macros/src/many_to_many.rs`
// module docs); two invocations with the same source/relation pair
// emit the same trait twice and trip rustc's E0428 / E0119
// duplicate-definition errors. The trait `ManyToMany<Group>` impl on
// `Person` is also redefined, compounding the failure.
//
// ## Collision-coverage scope (matters for whoever audits this fixture)
//
// rustc only catches **same-suffix** trait redefinitions. Two
// `many_to_many!` invocations with the same `(Source, relation)` (this
// fixture) share the `…ManyToManyRelation` suffix and collide at
// rustc.
//
// What rustc DOES NOT catch is a `many_to_many!` competing with a
// `reverse_one_to_many!` / `reverse_one_to_one!` for the same accessor
// name on the same source — the M2M side emits
// `…ManyToManyRelation`, the reverse side emits `…ReverseRelation`,
// the two trait names differ, both compile cleanly, and the conflict
// only surfaces as an "ambiguous method call" error at every
// downstream call site that has both traits in scope. That gap is
// closed by
// `djogi::relation::registry::validate_relation_accessor_collisions`,
// which adopters call once at startup or in a CI gate; see registry.rs
// module docs and the `validator_*` unit tests there for the contract.

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
// type. The emitted per-relation trait `PersonGroupsManyToManyRelation`
// (and its `impl... for Person`) is defined twice, tripping rustc's
// duplicate-definition check (E0428) and failing the build here. The
// `impl ManyToMany<Group> for Person` blanket also duplicates, so
// E0119 (conflicting trait implementation) fires alongside.
djogi::many_to_many!(
 Person, Group,
 through = PersonGroup,
 this_fk = person_id,
 that_fk = group_id,
 relation = "groups"
);

fn main() {}
