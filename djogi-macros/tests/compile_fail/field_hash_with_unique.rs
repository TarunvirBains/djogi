// v3 T2 + #83 — `#[field(unique, index = "<non-btree>")]`
// is rejected at the macro layer.
//
// Hash is the canonical case (and the original rejection).
// As of the rejection broadens to every non-btree access
// method, since PostgreSQL unique indexes are btree-only and mixing
// field-level `unique` with a non-btree `index = "<method>"` is
// ambiguous shorthand. The sibling
// `phase85_field_<method>_with_unique` fixtures cover `gin`, `gist`,
// `brin`, and `spgist` with the same diagnostic shape.
use djogi::prelude::*;

#[model(table = "posts")]
#[derive(Debug, Clone)]
pub struct Post {
 #[field(index = "hash", unique)]
 pub slug: String,
}

fn main() {}
