//! GH #227 Cluster A F2 — `protected(per_scope = { ... })` is scalar-only.
//!
//! Relation fields already project through `expose(scope -> Peer)`; attaching a
//! presentation codec to the relation slot itself must fail with a clear
//! compile-time diagnostic.
use djogi::prelude::*;

#[model(table = "phase85_227_per_scope_relation_owner")]
#[derive(Debug, Clone)]
pub struct Owner {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "phase85_227_per_scope_relation_vehicle", no_default)]
#[derive(Debug, Clone)]
pub struct Vehicle {
    #[field(
        expose(public -> OwnerPublic),
        protected(
            sensitivity = "pii",
            rationale = "owner relation should not accept codecs",
            per_scope = {
                public = {
                    presentation_codec = djogi::presentation::builtins::MaskString
                }
            }
        )
    )]
    pub owner: ForeignKey<Owner>,
}

fn main() {}
