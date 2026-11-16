//! Issue #318 — `{Model}Filter` null-only setters for nullable columns.
//!
//! Proves every outer-`Option<T>` field gains `<field>_is_null()` /
//! `<field>_is_not_null()` setters on its `{Model}Filter`, INCLUDING fields
//! whose inner type does not implement `IntoFilterValue`
//! (`Option<Jsonb<DocMeta>>` — `Jsonb<T>` has no `IntoFilterValue` impl).
//! These setters are value-free and must compile without any `IntoFilterValue`
//! bound on the inner type — parity with the closure API's
//! `f.<field>().is_null()`.
//!
//! Also proves the collision carve-out: a user field literally named
//! `flag_is_null` keeps its own generic setter, and no duplicate no-arg
//! `flag_is_null()` is emitted for the sibling `flag: Option<bool>`.
//!
//! Non-nullable fields (`title: String`) must NOT gain null setters — that
//! negative is pinned by the companion compile-fail fixture
//! `issue318_non_nullable_no_null_setter.rs`.
//!
//! Compile-only: no live Postgres pool. Function coercion pins the setter
//! signatures and the `filter_struct` composition.

use djogi::JsonbSchema;
use djogi::prelude::*;
use serde::{Deserialize, Serialize};

// Helper type for the Jsonb field — must derive JsonbSchema, Serialize,
// Deserialize, Default, Debug, Clone (matching the jsonb_schema.rs fixture
// pattern). `Jsonb<DocMeta>` has no `IntoFilterValue` impl, making it the
// genuinely non-portable inner for acceptance criterion 1.
#[derive(JsonbSchema, Serialize, Deserialize, Default, Debug, Clone)]
pub struct DocMeta {
    pub tag: String,
}

#[model(table = "issue318_docs")]
#[derive(Debug, Clone)]
pub struct Doc {
    // Non-nullable — generic setter only, no null setters.
    pub title: String,
    // Portable-optional inner — null setters + generic setter.
    pub summary: Option<String>,
    // Genuinely non-portable inner: `Jsonb<DocMeta>` has NO `IntoFilterValue`
    // impl, so the generic setter cannot be used with a value, but the null
    // setters still compile — this is the crux of acceptance criterion 1.
    pub meta: Option<Jsonb<DocMeta>>,
}

// Each null setter returns `Self` (DocFilter) and chains. Function coercion
// is the load-bearing typecheck.
#[allow(dead_code)]
fn _summary_null_setters_chain() -> DocFilter {
    DocFilter::new().summary_is_null().summary_is_not_null()
}

// The crux: a non-portable inner (`Option<Jsonb<DocMeta>>`, no `IntoFilterValue`
// impl) still gets working null setters with NO `IntoFilterValue` bound.
#[allow(dead_code)]
fn _meta_null_setters_compile() -> DocFilter {
    DocFilter::new().meta_is_null().meta_is_not_null()
}

// Null setters compose with value setters in one chain, and the whole filter
// flows into a `QuerySet` via `filter_struct`.
#[allow(dead_code)]
fn _mixed_chain_into_queryset() -> djogi::query::QuerySet<Doc> {
    let f = DocFilter::new()
        .title(Lookup::Eq("readme".to_string()))
        .summary_is_not_null()
        .meta_is_null();
    Doc::objects().filter_struct(f)
}

// Collision carve-out: a field literally named `flag_is_null` produces a
// GENERIC setter `flag_is_null<__V>(Lookup<__V>)`. The sibling
// `flag: Option<bool>` would normally generate a no-arg `flag_is_null()` null
// helper, but collision-skipping suppresses it to avoid a duplicate-method
// conflict. Calling `flag_is_null` WITH an argument proves it is the generic
// setter (not a no-arg null helper), and the fact that this compiles proves no
// duplicate no-arg `flag_is_null()` was emitted.
#[model(table = "issue318_collisions")]
#[derive(Debug, Clone)]
pub struct Collision {
    pub flag: Option<bool>,
    pub flag_is_null: bool,
    pub flag_is_not_null: bool,
    pub note: Option<String>,
}

#[allow(dead_code)]
fn _collision_field_keeps_generic_setter() -> CollisionFilter {
    // `flag_is_null` here is the generic Lookup setter for the bool column (not
    // a no-arg helper).
    CollisionFilter::new()
        .flag_is_null(Lookup::Eq(true))
        .flag_is_not_null(Lookup::Eq(false))
        // `note` is Option<String> and DOES get the no-arg null helpers.
        .note_is_null()
        .note_is_not_null()
}

fn main() {}
