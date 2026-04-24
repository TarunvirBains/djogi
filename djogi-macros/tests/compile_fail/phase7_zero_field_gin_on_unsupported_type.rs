// Phase 7-Zero v3 T2 — Q4: `#[field(index = "gin")]` is type-gated.
//
// GIN indexes only make sense on `Jsonb<T>`, `Vec<T>` (array-typed
// columns), and `tsvector` — any other field type triggers a compile
// error pointing at the model-level `#[model(indexes(...))]` syntax
// where opclass selection lives for advanced cases.
use djogi::prelude::*;

#[model(table = "posts")]
#[derive(Debug, Clone)]
pub struct Post {
    #[field(index = "gin")]
    pub view_count: i32,
}

fn main() {}
