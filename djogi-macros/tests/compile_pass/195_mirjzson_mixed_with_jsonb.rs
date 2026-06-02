// MirJzSON gate compile-pass fixture covering
// the mixed-JSONB shape: one typed `Jsonb<T>` column plus one
// `MirJzSON` column plus one `Option<MirJzSON>` column on the same
// model.
//
// Locks in three properties simultaneously:
//
// 1. `Jsonb<T>` is NOT subject to the `#[mirjzson(...)]` gate — the
//    typed schema IS the justification.
// 2. Multiple `MirJzSON` fields each carry their own justification.
// 3. The attribute parsing and stripping are independent per field;
//    other field types coexist freely.

use djogi::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(JsonbSchema, Serialize, Deserialize, Default, Debug, Clone)]
pub struct TypedMeta {
    pub view_count: i64,
    pub published: bool,
}

// `MirJzSON` is not `Default`, so the bare-field variant requires
// `no_default` to suppress the macro-emitted `Default` impl. The
// `Option<MirJzSON>` field would compile under the default `Default`
// path (via `Option::None`); we still pin the `no_default` shape
// because the bare `raw_request` field forces it.
#[model(table = "phase85_mirjzson_mixed_payloads", no_default)]
#[derive(Debug, Clone)]
pub struct MixedPayload {
    pub source: String,
    pub typed_meta: Jsonb<TypedMeta>,
    #[mirjzson(justification = "raw audit blob with shape varying per row")]
    pub raw_request: MirJzSON,
    #[mirjzson(justification = "optional response payload owned by external SDK")]
    pub raw_response: Option<MirJzSON>,
}

fn main() {}
