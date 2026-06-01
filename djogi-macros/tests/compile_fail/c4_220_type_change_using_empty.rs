// Cluster 4 djogi#220 — `#[field(type_change_using = "")]` is rejected.
//
// The USING expression must be non-empty / non-whitespace-only. An empty
// literal would lower to `USING ()` in the migration's
// `ALTER COLUMN ... TYPE ... USING (<expr>)` statement, which is invalid
// Postgres SQL and would surface only at apply time. The macro rejects the
// empty literal at parse time with a span-precise diagnostic pointing at
// the offending string — mirrors the `check` / `comment` validation
// shape (djogi#105 / djogi#217).

use djogi::prelude::*;

#[model(table = "items_220_empty", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Item220Empty {
    #[field(type_change_using = "")]
    pub kind: String,
}

fn main() {}
