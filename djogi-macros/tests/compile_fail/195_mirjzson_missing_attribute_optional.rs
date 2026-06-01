// MirJzSON gate compile-fail fixture for the
// `Option<MirJzSON>` shape.
//
// The gate fires on both `MirJzSON` and `Option<MirJzSON>`. The
// last-segment matcher strips one `Option<…>` layer before the type
// check, so the nullable shape is just as gated as the required shape.

use djogi::prelude::*;

#[model(table = "phase85_mirjzson_missing_optional")]
#[derive(Debug, Clone)]
pub struct OptionalPayload {
    pub source: String,
    pub payload: Option<MirJzSON>,
}

fn main() {}
