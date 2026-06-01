// `unique(..., using = "spgist")` is rejected.
//
// PostgreSQL unique indexes are btree-only. `CREATE UNIQUE INDEX … USING
// spgist` is rejected by the server with "access method does not support
// unique indexes", so accepting this declaration would compile a model
// whose generated migration SQL fails at apply.
use djogi::prelude::*;

#[model(table = "tags", indexes(
    unique(fields = [path], using = "spgist"),
))]
#[derive(Debug, Clone)]
pub struct Tag {
    pub path: String,
}

fn main() {}
