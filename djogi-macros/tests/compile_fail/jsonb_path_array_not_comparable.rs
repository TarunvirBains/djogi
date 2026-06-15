// djogi#372 — a JSONB array is not a scalar the cast table can target, so an
// array `Vec<V>` has no meaningful scalar JSONB-path comparison.
// `.path::<Vec<V>>(...)` may construct (dynamic-path escape hatch) but the
// comparison surface is gated by `JsonbPathComparable`, which `Vec<V>` does
// not implement. A `.eq(...)` on it must be a compile error.
//
// `Meta` derives `Default` so the `#[model]`-injected `impl Default` (which
// requires every field, including `Jsonb<Meta>`, to be `Default`) is
// satisfied — otherwise the fixture would fail with a `Default` error and
// mask the `JsonbPathComparable` error under test.
use djogi::prelude::*;
use djogi::JsonbSchema;
use serde::{Deserialize, Serialize};

#[derive(JsonbSchema, Serialize, Deserialize, Default, Debug, Clone)]
pub struct Meta {
 pub label: String,
}

#[model(table = "docs")]
#[derive(Debug, Clone)]
pub struct Doc {
 pub meta: djogi::Jsonb<Meta>,
}

fn _no_array_jsonb_compare() {
 let _ = Doc::objects().filter(|f| {
  f.meta()
  .explicit_pg_predicate()
  .path::<Vec<i32>>("tags")
  .eq(vec![1, 2])
 });
}

fn main() {}
