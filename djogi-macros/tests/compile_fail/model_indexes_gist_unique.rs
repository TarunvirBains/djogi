// `unique(..., using = "gist")` is rejected.
//
// PostgreSQL unique indexes are btree-only. `CREATE UNIQUE INDEX … USING
// gist` is rejected by the server with "access method does not support
// unique indexes", so accepting this declaration would compile a model
// whose generated migration SQL fails at apply.
//
// For row-exclusion semantics on a gist column (the legitimate use case
// users sometimes reach for here), declare an `EXCLUDE USING gist (…
// WITH &&)` constraint instead of a unique index.
use djogi::prelude::*;

#[model(table = "places", no_default, indexes(
 unique(fields = [location], using = "gist"),
))]
#[derive(Debug, Clone)]
pub struct Place {
 pub location: String,
}

fn main() {}
