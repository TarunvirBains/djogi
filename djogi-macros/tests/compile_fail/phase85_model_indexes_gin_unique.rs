// Phase 8.5 #83 — `unique(..., using = "gin")` is rejected.
//
// PostgreSQL unique indexes are btree-only. `CREATE UNIQUE INDEX … USING
// gin` is rejected by the server with "access method does not support
// unique indexes", so accepting this declaration would compile a model
// whose generated migration SQL fails at apply.
use djogi::prelude::*;
use serde_json::Value;

#[model(table = "profiles", indexes(
    unique(fields = [payload], using = "gin"),
))]
#[derive(Debug, Clone)]
pub struct Profile {
    pub payload: Jsonb<Value>,
}

fn main() {}
