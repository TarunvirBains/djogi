// Declaring two `reverse_one_to_many!` accessors with the same method
// name on the same receiver type must fail to compile. The macro does
// not (yet) perform a cross-invocation collision check in inventory
// — that lands in a follow-up phase — but each invocation emits a
// plain inherent method on the receiver, so two macros that produce
// the same method name on the same type hit rustc's
// duplicate-definition error directly. This fixture pins that failure
// mode so a later refactor that shifts the emission into an extension
// trait (which would silently lose the collision check) trips this
// trybuild fixture instead of slipping by.
//
// The test also documents the scope of the Phase 3 Task 7 collision
// guarantee: same-type / same-name accessors are blocked; cross-kind
// collisions (an FK reverse and an M2M accessor with the same method
// name) or same-name accessors via `reverse_one_to_one!` + `reverse_
// one_to_many!` trip the same duplicate-inherent-method error, so the
// coverage below is load-bearing for all three macros.
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
// The emitted `impl Owner { pub fn cars(...) }` collides with the
// first at rustc's duplicate-definition check, failing the build
// here.
djogi::reverse_one_to_many!(Owner, cars -> Vehicle by owner_id);

fn main() {}
