//! A model whose visage was renamed can still be embedded full-peer
//! (`expose(scope -> Model)`) and narrow (`expose(scope -> CanonicalName)`)
//! from another model, with the embedding model unedited relative to the
//! pre-rename world.
use djogi::prelude::*;
use djogi::query::internal::Condition;
use djogi::__private::serde::{Deserialize, Serialize};

#[model(table = "visage_rename_fullpeer_owner", visage_names(public = OwnerCard))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(crate = "::djogi::__private::serde")]
pub struct Owner {
    #[field(expose(public, admin))]
    pub name: String,
}

#[model(table = "visage_rename_fullpeer_vehicle", no_default)]
#[derive(Debug, Clone)]
pub struct Vehicle {
    #[field(expose(public))]
    pub plate: String,

    // Narrow embed via the CANONICAL name — unaffected by the rename
    // because `OwnerPublic` aliases `OwnerCard`.
    #[field(expose(public -> OwnerPublic))]
    pub owner: ForeignKey<Owner>,

    // Full-peer embed via the model ident — never touched renaming a visage.
    #[field(expose(admin -> Owner))]
    pub owner_admin: ForeignKey<Owner>,
}

/// Narrow-embed traversal composes through the peer's canonical `Fields`
/// alias. `VehiclePublic` is relation-nesting (no `::filter`), so the chain
/// is proven through a standalone helper that takes the embedder's `Fields`
/// — exactly as the shipped required/optional traversal fixtures do. The
/// embedder's own `Fields` type uses the ELIDED generic form
/// (`&VehiclePublicFields`, no `<Vehicle>`) because `{Visage}Fields` defaults
/// `RootModel = Source`, so the embedder's root binds to itself — matching the
/// corpus (`required_fk_traversal.rs:44`, `optional_fk_traversal.rs:50`).
#[allow(dead_code)]
fn narrow_traversal_composes(v: &VehiclePublicFields) -> Condition {
    v.owner().name().eq("Ada".to_string())
}

fn main() {
    let fields = VehiclePublicFields::default();

    // The narrow accessor's static return type names the peer's CANONICAL
    // `Fields` alias parameterized by the EMBEDDER root (`OwnerPublicFields<Vehicle>`),
    // which resolves through the rename to `OwnerCardFields<Vehicle>`. The
    // explicit `<Vehicle>` is REQUIRED here (not elided): the narrow accessor
    // emits `-> {Peer}Fields<{Root}>` with the embedder as the root
    // (`visage_fields.rs:244`), so the peer `Fields` is rooted on `Vehicle`,
    // NOT on its own default `RootModel = Owner`. Eliding to `OwnerPublicFields`
    // would default the root to `Owner` and fail to type-check against the
    // accessor's `OwnerPublicFields<Vehicle>` return. The corpus keeps this
    // binding explicit for the same reason (`optional_fk_traversal.rs:79`).
    // Naming the alias here is the load-bearing acceptance assertion: if the
    // canonical `Fields` alias did not resolve, this binding would fail.
    let _peer_fields: OwnerPublicFields<Vehicle> = fields.owner();

    // The traversal chain type-checks into a `Condition`.
    let _traversed: Condition = narrow_traversal_composes(&fields);
}
