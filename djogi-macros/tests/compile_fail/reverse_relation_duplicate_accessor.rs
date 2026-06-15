// Declaring two `reverse_one_to_many!` accessors with the same method
// name on the same receiver type must fail to compile. The macro emits
// a per-relation trait `{Receiver}{Method-pascal}ReverseRelation` plus
// its impl (GH issue #39 — coherence-rule rationale, see
// `djogi-macros/src/reverse_relation.rs` module docs), so two macros
// that would produce the same trait name on the same source hit
// rustc's E0428 / E0119 duplicate-definition errors directly.
//
// ## Collision-coverage scope (matters for whoever audits this fixture)
//
// rustc only catches **same-suffix** trait redefinitions. The reverse
// macros all emit the `…ReverseRelation` suffix, so:
//
// - two `reverse_one_to_many!` with the same `(Receiver, method)` (this
// fixture), or
// - one `reverse_one_to_many!` + one `reverse_one_to_one!` sharing the
// same `(Receiver, method)`,
//
// both trip the same E0428 / E0119 errors at the trait layer.
//
// What rustc DOES NOT catch is the cross-suffix case — a
// `reverse_one_to_many!` (or `reverse_one_to_one!`) plus a
// `many_to_many!` exposing the same accessor name on the same source.
// They emit DIFFERENT trait names (`…ReverseRelation` vs
// `…ManyToManyRelation`), both compile cleanly, and the collision only
// surfaces as an "ambiguous method call" error at every downstream
// call site that has both traits in scope. That gap is closed by
// `djogi::relation::registry::validate_relation_accessor_collisions`,
// which adopters call once at startup or in a CI gate; see registry.rs
// module docs and the `validator_*` unit tests there for the contract.
use djogi::prelude::*;

#[model(table = "owners_dup")]
#[derive(Debug, Clone)]
pub struct Owner {
 pub name: String,
}

#[model(table = "vehicles_dup", no_default)]
#[derive(Debug, Clone)]
pub struct Vehicle {
 pub make: String,
 pub owner_id: ForeignKey<Owner>,
}

// First declaration — legitimate.
djogi::reverse_one_to_many!(Owner, cars -> Vehicle by owner_id);

// Second declaration with the same method name on the same receiver.
// The emitted per-relation trait `OwnerCarsReverseRelation` (and its
// `impl... for Owner`) is defined twice, tripping rustc's
// duplicate-definition check (E0428 on the trait, E0119 on the impl)
// and failing the build here.
djogi::reverse_one_to_many!(Owner, cars -> Vehicle by owner_id);

fn main() {}
