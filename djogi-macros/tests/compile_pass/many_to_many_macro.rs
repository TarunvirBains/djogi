// Verifies that `djogi::many_to_many!` stamps out the trait impl, the
// named accessor, and the inventory marker — the shape documented in
// `djogi-macros/src/many_to_many.rs`.
//
// This fixture is the macro-side twin of `many_to_many_hand_impl.rs`:
// that fixture pins the hand-written reference impl, this one pins
// that the macro output is congruent with it. A reviewer can diff the
// two fixtures to see what the macro is doing on behalf of the user.
//
// Pinned invariants (all compile-time):
//
// - The macro emits a complete `impl ManyToMany<Group> for Person`
//  — all four associated items (`Through`, `RELATION`, `this_fk`,
//  `that_fk`) and all three required async methods. Missing one
//  would fail rustc's incomplete-impl check.
// - The inherent accessor `person.groups(...)` exists on `Person`
//  with the return shape `impl Future<Output = Result<Vec<Group>, _>> + Send`.
//  The `_return_type_is_vec_group` probe pins that type without
//  requiring a live Postgres pool at runtime — the probe only has
//  to typecheck, not execute.
// - The inventory marker is submitted with `kind = M2M`, the right
//  source / target / name / via — the projection generator
//  will find it in the same walk that finds reverse-relation
//  markers.
// - Two `many_to_many!` invocations for opposite directions
//  (`Person → Group` and `Group → Person`) compile without
//  conflict. Each direction picks its own relation name; symmetry
//  is a data-model property, not a macro-invocation property.
//
// No live Postgres — the bodies below never call `.await`, only
// typecheck.

use djogi::prelude::*;
use djogi::relation::{ForeignKey, ManyToMany};

#[model(table = "persons_mmm")]
#[derive(Debug, Clone)]
pub struct Person {
 pub name: String,
}

#[model(table = "groups_mmm")]
#[derive(Debug, Clone)]
pub struct Group {
 pub name: String,
}

// `through` marks this as a junction model; `no_default` because
// `ForeignKey<T>` has no `Default` impl (a relation with no target is
// meaningless).
#[model(table = "person_groups_mmm", through, no_default)]
#[derive(Debug, Clone)]
pub struct PersonGroup {
 pub person_id: ForeignKey<Person>,
 pub group_id: ForeignKey<Group>,
 pub role: String,
}

// One invocation per direction. Each picks its own relation name —
// "groups" for `Person → Group` and "members" for `Group → Person` —
// matching the project's explicit-over-implicit stance on accessor
// naming.
djogi::many_to_many!(
 Person, Group,
 through = PersonGroup,
 this_fk = person_id,
 that_fk = group_id,
 relation = "groups"
);

djogi::many_to_many!(
 Group, Person,
 through = PersonGroup,
 this_fk = group_id,
 that_fk = person_id,
 relation = "members"
);

// Compile-only probes — one per invariant the macro is supposed to
// preserve. Each probe is a function we never call; if it typechecks
// the invariant holds.

fn _named_accessor_returns_vec_target<'a>(
 person: &'a Person,
 ctx: &'a mut DjogiContext,
) -> impl std::future::Future<Output = Result<Vec<Group>, DjogiError>> + Send + 'a {
 // The macro emits `pub fn groups<'ctx>(...) -> impl Future<...>`;
 // coercing it to this named future type pins the return shape.
 person.groups(ctx)
}

fn _reverse_direction_named_accessor<'a>(
 group: &'a Group,
 ctx: &'a mut DjogiContext,
) -> impl std::future::Future<Output = Result<Vec<Person>, DjogiError>> + Send + 'a {
 // Symmetric probe for the reverse direction — both invocations
 // emitted independent accessors.
 group.members(ctx)
}

fn _trait_const_and_fks_are_pinned() {
 // The macro wired `RELATION` / `this_fk` / `that_fk` to the
 // concrete strings from the invocation. Calling them through the
 // trait probes the trait impl directly — a macro that silently
 // dropped any associated item would fail this.
 let _name: &'static str = <Person as ManyToMany<Group>>::RELATION;
 let _this: &'static str = <Person as ManyToMany<Group>>::this_fk();
 let _that: &'static str = <Person as ManyToMany<Group>>::that_fk();
}

fn _through_associated_type_resolves_to_junction() {
 // `type Through = PersonGroup` was picked up from the macro's
 // `through = PersonGroup` argument; calling `table_name()` on it
 // confirms the associated type points at a real `Model`.
 fn _is_model<M: Model>() -> &'static str {
  M::table_name()
 }
 let _ = _is_model::<<Person as ManyToMany<Group>>::Through>();
 let _ = _is_model::<<Group as ManyToMany<Person>>::Through>();
}

fn main() {
 // Runtime sanity — pin the concrete string values from each
 // direction. An accidental `this_fk` / `that_fk` swap in either
 // emission site would flip these assertions.
 assert_eq!(<Person as ManyToMany<Group>>::RELATION, "groups");
 assert_eq!(<Person as ManyToMany<Group>>::this_fk(), "person_id");
 assert_eq!(<Person as ManyToMany<Group>>::that_fk(), "group_id");

 assert_eq!(<Group as ManyToMany<Person>>::RELATION, "members");
 assert_eq!(<Group as ManyToMany<Person>>::this_fk(), "group_id");
 assert_eq!(<Group as ManyToMany<Person>>::that_fk(), "person_id");

 // Both associated `Through` types resolve to `PersonGroup` — the
 // junction is shared across directions.
 assert_eq!(
  <<Person as ManyToMany<Group>>::Through as Model>::table_name(),
  "person_groups_mmm"
 );
 assert_eq!(
  <<Group as ManyToMany<Person>>::Through as Model>::table_name(),
  "person_groups_mmm"
 );

 // The junction flag flows through to the descriptor exactly as
 // the hand-impl fixture verifies.
 assert!(<PersonGroup as Model>::descriptor().is_through);
 assert!(!<Person as Model>::descriptor().is_through);
 assert!(!<Group as Model>::descriptor().is_through);

 // Walk the inventory slice for the macro's M2M markers. Both
 // directions should be present with `kind = M2M`; the `via`
 // carries the `this_fk` column for that direction, matching the
 // documented convention (reads this field as "how do I
 // reach the accessor from the source?").
 use djogi::relation::registry::{RelationKind, ReverseRelationMarker};

 let mut saw_person_groups = false;
 let mut saw_group_members = false;
 for marker in djogi::__private::inventory::iter::<ReverseRelationMarker> {
  if marker.source() == "Person" && marker.name() == "groups" {
   assert_eq!(marker.kind(), RelationKind::M2M);
   assert_eq!(marker.target(), "Group");
   assert_eq!(marker.via(), "person_id");
   saw_person_groups = true;
  }
  if marker.source() == "Group" && marker.name() == "members" {
   assert_eq!(marker.kind(), RelationKind::M2M);
   assert_eq!(marker.target(), "Person");
   assert_eq!(marker.via(), "group_id");
   saw_group_members = true;
  }
 }
 assert!(
  saw_person_groups,
  "many_to_many! did not register the `Person::groups` accessor"
 );
 assert!(
  saw_group_members,
  "many_to_many! did not register the `Group::members` accessor"
 );
}
