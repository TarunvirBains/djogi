// djogi#372 — a JSONB range is stored as an object, not a scalar the cast
// table can target, so `Range<T>` has no meaningful scalar JSONB-path
// comparison. `.path::<Range<T>>(...)` may construct (dynamic-path escape
// hatch) but the comparison surface is gated by `JsonbPathComparable`, which
// `Range<T>` does not implement. A `.eq(...)` on it must be a compile error.
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

fn _no_range_jsonb_compare() {
    let _ = Doc::objects().filter(|f| {
        f.meta()
            .explicit_pg_predicate()
            .path::<djogi::Range<i32>>("span")
            .eq(djogi::Range::<i32>::default())
    });
}

fn main() {}
