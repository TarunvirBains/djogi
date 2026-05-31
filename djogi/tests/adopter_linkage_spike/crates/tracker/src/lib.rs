//! Tracker crate — defines Elephant and Herd models.
//!
//! Both models use `#[model]` which emits `inventory::submit!(ModelDescriptor { ... })`.
//! This crate is the "two-models-in-one-crate" test subject for linker linkage analysis.

use djogi::prelude::*;

/// An elephant model — used to test single-type reference linkage.
#[model(table = "tracker_elephant")]
pub struct Elephant {
    pub name: String,
    pub weight_kg: i64,
}

/// A herd model — co-resides in the same crate as Elephant.
/// Used to test whether referencing Elephant also forces Herd's descriptor
/// into the linked binary via inventory.
#[model(table = "tracker_herd")]
pub struct Herd {
    pub herd_name: String,
    pub size: i32,
}
