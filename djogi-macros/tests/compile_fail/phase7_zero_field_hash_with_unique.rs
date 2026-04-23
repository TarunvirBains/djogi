// Phase 7-Zero v3 T2 — Q3: hash indexes reject UNIQUE at declaration.
//
// `#[field(index = "hash", unique)]` combines two incompatible Postgres
// features — hash indexes cannot enforce uniqueness. The macro must
// reject this combination at compile time with an error that names the
// specific field and the specific rule violated.
use djogi::prelude::*;

#[model(table = "posts")]
#[derive(Debug, Clone)]
pub struct Post {
    #[field(index = "hash", unique)]
    pub slug: String,
}

fn main() {}
