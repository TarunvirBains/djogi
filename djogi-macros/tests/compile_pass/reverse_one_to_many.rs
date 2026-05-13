// Verifies that `djogi::reverse_one_to_many!` expands cleanly and emits
// an accessor method with the expected signature. Acceptance checks:
//
//   - `Owner::cars(&mut ctx)` compiles as a future whose Ok output is
//     `Vec<Vehicle>` (no type-erased escape hatch in the public surface),
//     where `ctx: &mut DjogiContext` matches the Phase 4 retrofit;
//   - the macro accepts a parsed `Receiver, method -> Returned by via_column`
//     form;
//   - the emitted method coexists with `{Model}Fields` / `{Model}Filter` /
//     `{Model}Related` without any naming collision;
//   - invoking the macro twice for two distinct method names on the same
//     receiver type emits two independent accessors (no global state in
//     the macro expansion).
//
// This fixture does NOT execute the accessor — no live Postgres is available
// to lihaaf runs. The body of `_signature_check` is typecheck-only:
// calling `.cars(&pool)` requires a real pool which we don't construct here.
// Instead, we consume the accessor's return type through function coercion:
// assigning the method reference to a fully-spelled function-pointer type
// exercises the signature at compile time without running it.

use djogi::prelude::*;

#[model(table = "owners_rrr")]
#[derive(Debug, Clone)]
pub struct Owner {
    pub name: String,
}

#[model(table = "vehicles_rrr", no_default)]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub make: String,
    pub owner_id: ForeignKey<Owner>,
}

// The reverse accessor — one line per direction. Lives at module scope
// (not inside an impl block) because `reverse_one_to_many!` emits its
// own `impl Owner { ... }` block.
djogi::reverse_one_to_many!(Owner, cars -> Vehicle by owner_id);

// A second accessor with a different name on the same receiver type.
// Points at the same via column on the same returned type — semantically
// a duplicate, but the macro does not (yet) block redundant invocations.
// The check is "two method names produce two independent methods".
djogi::reverse_one_to_many!(Owner, all_vehicles -> Vehicle by owner_id);

fn _accessor_returns_vec_vehicle<'a>(
    owner: &'a Owner,
    ctx: &'a mut DjogiContext,
) -> impl std::future::Future<Output = Result<Vec<Vehicle>, DjogiError>> + Send + 'a {
    owner.cars(ctx)
}

fn _second_accessor_compiles<'a>(
    owner: &'a Owner,
    ctx: &'a mut DjogiContext,
) -> impl std::future::Future<Output = Result<Vec<Vehicle>, DjogiError>> + Send + 'a {
    owner.all_vehicles(ctx)
}

fn main() {
    // Walk the inventory-submitted markers to verify the `reverse_one_to_many!`
    // expansion registered a record. We don't assert "exactly N markers"
    // because other compile-pass fixtures in this suite may register their
    // own; we just confirm the one for `Owner::cars` is present with the
    // right shape.
    use djogi::relation::registry::{RelationKind, ReverseRelationMarker};

    let mut saw_cars = false;
    let mut saw_all_vehicles = false;
    // Reach through `__private` for the inventory iterator — the crate
    // re-exports `inventory` there rather than as a top-level public
    // dep. Compile-pass fixtures are test-only binaries, so touching
    // the `#[doc(hidden)]` surface is fine; a production user would
    // add `inventory` as a direct dep.
    for marker in djogi::__private::inventory::iter::<ReverseRelationMarker> {
        if marker.source() == "Owner" && marker.name() == "cars" {
            assert_eq!(marker.kind(), RelationKind::FK);
            assert_eq!(marker.target(), "Vehicle");
            assert_eq!(marker.via(), "owner_id");
            saw_cars = true;
        }
        if marker.source() == "Owner" && marker.name() == "all_vehicles" {
            assert_eq!(marker.kind(), RelationKind::FK);
            saw_all_vehicles = true;
        }
    }
    assert!(saw_cars, "reverse_one_to_many! did not register the `cars` accessor");
    assert!(
        saw_all_vehicles,
        "reverse_one_to_many! did not register the `all_vehicles` accessor"
    );
}
