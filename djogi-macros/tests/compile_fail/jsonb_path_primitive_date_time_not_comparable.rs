// djogi#372 — `time::PrimitiveDateTime` serializes into a JSONB column as a
// numeric tuple (e.g. [2021,2,3,4,5,0]) rather than a timestamp string,
// because the workspace does not enable `time/serde-human-readable`. A
// `(col->>'key')::timestamp` cast on that tuple text would fail at the
// database, so `PrimitiveDateTime` is blocked from the JSONB-path comparison
// surface: it has no `JsonbPathComparable` impl. A `.eq(...)` on it must be a
// compile error, not a runtime cast error.
//
// `Meta` derives `Default` so the `#[model]`-injected `impl Default` (which
// requires every field, including `Jsonb<Meta>`, to be `Default`) is
// satisfied — otherwise the fixture would fail with a `Default` error and
// mask the `JsonbPathComparable` error under test.
//
// The `time/macros` feature is NOT enabled, so `time::macros::datetime!` is
// unavailable; construct the probe via the calendar constructors so the only
// error in the blessed `.stderr` is the trait-bound error, not a
// value-construction error.
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

fn _no_primitive_date_time_jsonb_compare() {
 let probe = time::PrimitiveDateTime::new(
  time::Date::from_calendar_date(2021, time::Month::January, 2).unwrap(),
  time::Time::from_hms(3, 4, 5).unwrap(),
 );
 let _ = Doc::objects().filter(|f| {
  f.meta()
  .explicit_pg_predicate()
  .path::<time::PrimitiveDateTime>("seen_at")
  .eq(probe)
 });
}

fn main() {}
