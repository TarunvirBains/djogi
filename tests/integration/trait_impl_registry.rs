//! .6 — Integration test for the trait_registry registration path.
//!
//! Exercises the end-to-end pipeline through `#[djogi::trait_impl]`:
//!
//! 1. Adopter declares a trait + a model + a `#[djogi::trait_impl]`
//!  impl block.
//! 2. The macro emits the impl unchanged + the per-impl carrier
//!  struct + the type-erased caster + the `inventory::submit!`
//!  registration.
//! 3. `djogi::trait_registry::iter_for_trait::<dyn Trait>()` walks
//!  the registry, filters by the trait's `TypeId`, yields the
//!  matching `&'static TraitRegistration` entries.
//! 4. The caster (.3 safe carrier pattern) round-trips an
//!  `Arc<Vehicle>` through `Arc<dyn Any + Send + Sync>` ↔
//!  `Arc<TraitImplCarrier<dyn Searchable>>` and recovers a working
//!  `Arc<dyn Searchable + Send + Sync>` for adopter use.
//!
//! The cross-type Sassi-consumer query test
//! is deferred to a later integration suite (it depends on
//! `DjogiContext::punnu<T>()`). This file ships only the
//! registration-side coverage here.
//!
//! No DB required — this test exercises only the inventory + caster
//! code paths.

use std::any::TypeId;
use std::sync::Arc;

trait Searchable {
    #[allow(dead_code)] // Method exercised through `Arc<dyn Searchable>` round-trip in .3 caster path.
    fn searchable_columns(&self) -> &'static [&'static str];
}

#[derive(Debug, Clone)]
pub struct Vehicle {
    pub title: String,
}

#[djogi::trait_impl]
impl Searchable for Vehicle {
    fn searchable_columns(&self) -> &'static [&'static str] {
        &["title"]
    }
}

#[derive(Debug, Clone)]
pub struct Person {
    pub name: String,
}

#[djogi::trait_impl]
impl Searchable for Person {
    fn searchable_columns(&self) -> &'static [&'static str] {
        &["name"]
    }
}

#[test]
fn iter_for_trait_yields_both_registrations() {
    // Both Vehicle and Person register Searchable; the registry must
    // surface both. Use `>=` rather than `==` because the inventory
    // is process-global — other tests in the same binary may register
    // additional Searchable impls.
    let count = djogi::trait_registry::iter_for_trait::<dyn Searchable>().count();
    assert!(
        count >= 2,
        "expected at least 2 Searchable registrations (Vehicle + Person), got {count}",
    );

    let names: Vec<&'static str> = djogi::trait_registry::iter_for_trait::<dyn Searchable>()
        .map(|r| r.model_type_name)
        .collect();
    assert!(names.contains(&"Vehicle"), "got: {names:?}");
    assert!(names.contains(&"Person"), "got: {names:?}");
}

#[test]
fn iter_for_trait_filters_by_type_id() {
    // Different trait — should yield zero registrations even though
    // the inventory has Vehicle and Person registered for Searchable.
    #[allow(dead_code)] // Marker trait — registry filter test only exercises the TypeId path.
    trait Sortable {
        fn sort_key(&self) -> &'static str;
    }
    let count = djogi::trait_registry::iter_for_trait::<dyn Sortable>().count();
    // No `#[djogi::trait_impl] impl Sortable` blocks reachable from
    // this test — should be zero.
    assert_eq!(
        count, 0,
        "expected zero Sortable registrations, got {count}",
    );
}

#[test]
fn caster_round_trips_arc_to_trait_object() {
    // Find the Vehicle registration and exercise its caster end-to-end:
    //
    //  1. Wrap a Vehicle instance in `Arc<dyn Any + Send + Sync>`.
    //  2. Call `(reg.caster)(&erased_arc)` → `Arc<dyn Any>`.
    //  3. Downcast to the per-impl carrier struct.
    //  4. Call the carrier's `into_arc()` to recover
    //   `Arc<dyn Searchable + Send + Sync>`.
    //  5. Call a trait method on the recovered Arc.
    //
    // The per-impl carrier type name is internal to the macro
    // expansion — we cannot reach it directly. But we CAN verify the
    // caster returns Some(_) for the matching type and None for a
    // mismatched type. The full round-trip-to-trait-object path is
    // deferred to 8δ (it goes through `Sassi::all_impl::<dyn T>()`,
    // which has the type-aware downcast helper threaded through).
    let vehicle: Vehicle = Vehicle {
        title: "Fast One".to_string(),
    };
    let arc_vehicle: Arc<dyn std::any::Any + Send + Sync> = Arc::new(vehicle);

    // Find the Vehicle Searchable registration.
    let reg = djogi::trait_registry::iter_for_trait::<dyn Searchable>()
        .find(|r| r.model_type_name == "Vehicle")
        .expect("Vehicle registered for Searchable");

    // Call the caster — must return Some(_) for the matching type.
    let result = (reg.caster)(&arc_vehicle);
    assert!(result.is_some(), "caster should match Vehicle's type");

    // Mismatched type — wrap a Person in the erased Arc and pass it
    // to Vehicle's caster. Should return None.
    let person_arc: Arc<dyn std::any::Any + Send + Sync> = Arc::new(Person {
        name: "Alice".to_string(),
    });
    let mismatched = (reg.caster)(&person_arc);
    assert!(mismatched.is_none(), "caster should reject mismatched type");
}

#[test]
fn registration_carries_correct_type_ids() {
    let reg = djogi::trait_registry::iter_for_trait::<dyn Searchable>()
        .find(|r| r.model_type_name == "Vehicle")
        .expect("Vehicle registered for Searchable");
    assert_eq!((reg.model_type_id)(), TypeId::of::<Vehicle>());
    assert_eq!((reg.trait_type_id)(), TypeId::of::<dyn Searchable>());
}
