// relation/visage traversal is SQL-only after the flip.
//
// Pre-PR3, `{Model}Fields` carried an optional SQL-alias path so
// relation accessors (`f.department().name()`) could compose dotted
// column references. PR3 moves that path-aware surface onto the
// SQL-only sibling `{Model}SqlFields` and leaves the root `{Model}Fields`
// as a portable ZST. Cached root rows do NOT carry joined relation
// values, so traversal predicates are SQL-only by construction —
// reachable through `{Model}SqlFields::with_path(...)` for macro-emitted
// SQL paths, and absent from the closure-API root surface.
//
// This fixture asserts the routing: the root `department` FK column
// accessor still exists, but it returns a `DjogiField<Employee,
// ForeignKey<Department>>` scalar handle, not a path-aware traversal
// handle. Adopters who try to walk `f.department().name()` inside a
// Punnu `filter_basic(...)` closure get a clear "no method named `name`
// found for `DjogiField<...>`" error. Traversal accessors live on the
// SQL-only visage/relation surfaces, not on the root portable field bag.
//
// Per the lihaaf compile-fixture contract, every lihaaf fixture has
// `fn main` so `.stderr` does not pick up E0601 noise.

use djogi::cache::*;
use djogi::prelude::*;

#[model(table = "phase8eta_traversal_dept")]
#[derive(Debug, Clone)]
pub struct Department {
    pub name: String,
}

#[model(table = "phase8eta_traversal_emp", no_default)]
#[derive(Debug, Clone)]
pub struct Employee {
    pub display_name: String,
    pub department: ForeignKey<Department>,
}

fn main() {
    let punnu = Punnu::<Employee>::builder().build();
    let _scope = punnu
        .scope(Vec::<MemQ<Employee>>::new())
        // Traversal `f.department().name()` is SQL-only after PR3. The
        // root FK column accessor returns `DjogiField<_, ForeignKey<_>>`,
        // so the compile error must surface when trying to continue into
        // `.name()` as though it were a path-aware traversal handle.
        .filter_basic(|f| f.department().name().eq("Sales"));
}
