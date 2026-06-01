// Cluster 4 djogi#216 Piece A — `#[field(domain = "...")]` on
// an `Option<ForeignKey<T>>` field is rejected.
//
// `detect_relation` unwraps one layer of `Option<…>` before classifying
// the inner type. This fixture pins the optional-FK path through the
// guard at `attrs.rs::3131` so a future refactor of the validation
// block cannot silently drop coverage for the optional-wrapped variant.

use djogi::prelude::*;

#[model(table = "owners_216_opt", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Owner216Opt {
    pub name: String,
}

#[model(table = "assets_216_opt", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Asset216Opt {
    #[field(domain = "positive_amount")]
    pub owner: Option<ForeignKey<Owner216Opt>>,
}

fn main() {}
