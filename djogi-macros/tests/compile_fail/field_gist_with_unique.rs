// `#[field(unique, index = "gist")]` is rejected.
//
// PostgreSQL unique indexes are btree-only, so a non-btree
// `index = "gist"` combined with `unique` is ambiguous. For row-overlap
// exclusion semantics on a gist column declare an `EXCLUDE USING gist
// (… WITH &&)` constraint at the model level instead.
use djogi::prelude::*;

#[model(table = "places")]
#[derive(Debug, Clone)]
pub struct Place {
    #[field(index = "gist", unique)]
    pub location: String,
}

fn main() {}
