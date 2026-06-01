// The `RelationPath` constructor is sealed against downstream fabrication.
// Prior to Task 4's follow-up fix, `RelationPath::__new` was `pub`
// (with `#[doc(hidden)]`), which let any downstream crate build a path
// whose `source_column` / `target_table` fields carried SQL-injection
// payloads — those strings then flowed straight into the
// `SqlAccumulator::push_sql` calls inside the prefetch and select_related
// emitters. The seal closes that vector.
//
// This test pins the seal at the type system: downstream code must not
// be able to call the path's constructor. `RelationPath::new` is now
// `pub(crate)` in the djogi crate, so naming it from outside the crate
// fails to resolve. The proc-macro-emitted `{Source}Related` accessors
// reach the constructor through a `#[doc(hidden)] pub` helper that
// validates identifier characters before instantiating the path — see
// `djogi::relation::__private::__make_relation_path`.
use djogi::prelude::*;
use djogi::relation::{ForeignKey, RelationKind, RelationPath};

#[model(table = "owners_seal_test")]
#[derive(Debug, Clone)]
pub struct Owner {
    pub name: String,
}

// `no_default` lets the struct carry non-`Default` field types without
// trying to derive a `Default` impl (ForeignKey has no `Default`).
#[model(table = "vehicles_seal_test", no_default)]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub make: String,
    pub owner_id: ForeignKey<Owner>,
}

fn main() {
    // This must not compile — `RelationPath::new` is `pub(crate)` in the
    // djogi crate, so the attempted downstream call resolves to a
    // private associated function. That is the compile-time half of
    // the seal; the runtime half is `__make_relation_path`'s identifier
    // validation (which would panic on the injection payload below even
    // if the caller reached it).
    let _: RelationPath<Vehicle, Owner> = RelationPath::new(
        "owner_id) OR 1=1 --",
        "owners_seal_test",
        RelationKind::ForeignKey,
    );
}
