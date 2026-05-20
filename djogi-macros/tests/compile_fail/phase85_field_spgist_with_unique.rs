// Phase 8.5 #83 — `#[field(unique, index = "spgist")]` is rejected.
//
// PostgreSQL unique indexes are btree-only, so a non-btree
// `index = "spgist"` combined with `unique` is ambiguous.
use djogi::prelude::*;

#[model(table = "tags")]
#[derive(Debug, Clone)]
pub struct Tag {
    #[field(index = "spgist", unique)]
    pub path: String,
}

fn main() {}
