// T11 / issue #30 — `Serialize` / `Deserialize` derives
// reach adopter code through `djogi::prelude::*`. A typed JSONB schema
// (`Jsonb<MyShape>`) derives serde directly on `MyShape`; the adopter
// should not need a separate `use serde::*` line or a direct `serde`
// dependency in their `Cargo.toml`. Pulling the derives out of
// `djogi::prelude::*` proves both are reachable.

use djogi::prelude::*;

#[derive(JsonbSchema, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Payload {
    pub foo: String,
    pub bar: i64,
}

fn main() {}
