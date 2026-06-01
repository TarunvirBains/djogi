// `unique(..., using = "brin")` is rejected.
//
// PostgreSQL unique indexes are btree-only. `CREATE UNIQUE INDEX … USING
// brin` is rejected by the server with "access method does not support
// unique indexes", so accepting this declaration would compile a model
// whose generated migration SQL fails at apply.
use djogi::prelude::*;

#[model(table = "events", no_default, indexes(
    unique(fields = [happened_at], using = "brin"),
))]
#[derive(Debug, Clone)]
pub struct Event {
    pub happened_at: DateTime,
}

fn main() {}
