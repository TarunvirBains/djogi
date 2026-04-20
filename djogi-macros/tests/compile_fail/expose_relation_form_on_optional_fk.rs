//! Relation-form `expose(public = "OwnerSummary")` on an
//! `Option<ForeignKey<T>>` field is deferred in Phase 4.5 — a follow-up
//! phase will lift this restriction once cross-model dispatch of
//! `Option<&T>` → peer projection is designed.
use djogi::prelude::*;

#[model(table = "owners_erfof")]
#[derive(Debug, Clone)]
pub struct Owner {
    pub name: String,
}

#[model(table = "vehicles_erfof", no_default)]
#[derive(Debug, Clone)]
pub struct Vehicle {
    #[field(expose(public = "OwnerSummary"))]
    pub owner: Option<ForeignKey<Owner>>,
}

fn main() {}
