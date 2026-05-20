// Phase 8.5 #83 — `#[field(unique, index = "brin")]` is rejected.
//
// PostgreSQL unique indexes are btree-only, so a non-btree
// `index = "brin"` combined with `unique` is ambiguous.
use djogi::prelude::*;

#[model(table = "events")]
#[derive(Debug, Clone)]
pub struct Event {
    #[field(index = "brin", unique)]
    pub happened_at: DateTime,
}

fn main() {}
