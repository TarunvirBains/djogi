// djogi#161 — `#[jsonb(scalar = "...")]` must be rejected.
//
// The `#[jsonb(scalar)]` escape hatch is a bare-word marker; admitting
// a value form would invite adopters to pass arbitrary SQL cast text
// through the macro layer. Postgres cast selection must come from
// `FieldType: IntoFilterValue`'s typed `jsonb_sql_cast` dispatch, not
// adopter-supplied strings. The macro rejects the value form at
// derive-expansion time with a span-anchored diagnostic.

use serde::{Deserialize, Serialize};

#[derive(djogi::JsonbSchema, Serialize, Deserialize, Default, Debug, Clone)]
pub struct BadSpec {
    #[jsonb(scalar = "::int8")]
    pub id: i64,
}

fn main() {}
