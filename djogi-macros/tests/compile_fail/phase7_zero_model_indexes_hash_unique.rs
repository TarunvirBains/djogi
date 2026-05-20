// Phase 7-Zero v3 T3 / Phase 8.5 #83 — `unique(..., using = "<non-btree>")`
// is rejected because PostgreSQL unique indexes are btree-only.
//
// Historical note: the original Phase 7-Zero rule rejected only
// `using = "hash"` with unique, on the narrower theory that hash indexes
// physically cannot enforce uniqueness. Phase 8.5 #83 generalises this:
// every non-btree access method (gin / gist / brin / spgist as well as
// hash) is rejected, because PostgreSQL's `CREATE UNIQUE INDEX … USING
// <method>` only supports btree. Hash stays in the rejection set as the
// canonical adopter-named method; the sibling `phase85_model_indexes_*`
// fixtures cover gin / gist / brin / spgist with the same diagnostic
// shape.
use djogi::prelude::*;

#[model(table = "users", indexes(
    unique(fields = [email], using = "hash"),
))]
#[derive(Debug, Clone)]
pub struct User {
    pub email: String,
}

fn main() {}
