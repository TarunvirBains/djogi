// djogi#216 — `#[field(domain = "x",
// max_length = N)]` is rejected.
//
// The domain provides its own column constraints (Postgres
// `CREATE DOMAIN <name> AS <base> [CHECK (...)]` carries length /
// range / regex checks baked into the type definition). Layering a
// `VARCHAR(N)` on top of the domain reference would emit contradictory
// column DDL: the domain's `AS <base>` clause already pins the
// underlying type, and `VARCHAR(N)` would override it at the column
// level — defeating the point of using the domain in the first place.

use djogi::prelude::*;

#[model(table = "tags_216_maxlen", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Tag216MaxLen {
    #[field(domain = "email_address", max_length = 64)]
    pub email: String,
}

fn main() {}
