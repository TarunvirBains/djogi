// Built-in PK-typed values (HeerId, RanjId, HeerIdDesc,
// RanjIdDesc, HeerIdRecencyBiased) are usable as ordinary ambient fields
// outside the framework-injected `id` slot. The `#[model]` macro must not
// special-case these types when they appear as user-declared fields.

use djogi::prelude::*;

#[model(table = "edges")]
#[derive(Debug, Clone)]
pub struct Edge {
    // `id` is `HeerIdDesc` (the post-T2 default); the three fields below
    // exercise the other built-in PK shapes in the ambient-field position.
    pub from_heerid: ::djogi::types::HeerId,
    pub to_ranjid: ::djogi::types::RanjId,
    pub to_recency: ::djogi::types::HeerIdRecencyBiased,
}

fn _ambient_surface(e: &Edge) {
    // Each ambient field decodes and serializes through the same
    // postgres-types codec path as any other scalar — no PK-slot handling.
    let _h: &::djogi::types::HeerId = &e.from_heerid;
    let _r: &::djogi::types::RanjId = &e.to_ranjid;
    let _rb: &::djogi::types::HeerIdRecencyBiased = &e.to_recency;
}

fn main() {}
