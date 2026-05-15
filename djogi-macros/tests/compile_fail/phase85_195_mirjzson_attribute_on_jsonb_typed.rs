// Phase 8.5 issue #195 — MirJzSON gate compile-fail fixture.
//
// `Jsonb<T>` is the typed-schema sibling of `MirJzSON` and is NOT
// subject to the justification gate — the typed schema IS the
// justification. Putting `#[mirjzson(...)]` on a `Jsonb<T>` field is
// therefore rejected with the same "only valid on `MirJzSON` /
// `Option<MirJzSON>`" diagnostic the wrong-type fixture exercises.

use djogi::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(JsonbSchema, Serialize, Deserialize, Default, Debug, Clone)]
pub struct TypedMeta {
    pub view_count: i64,
    pub published: bool,
}

#[model(table = "phase85_mirjzson_attribute_on_jsonb_typed")]
#[derive(Debug, Clone)]
pub struct JsonbTyped {
    #[mirjzson(justification = "Jsonb<T> is typed and does not need this gate")]
    pub meta: Jsonb<TypedMeta>,
}

fn main() {}
