// Phase 8.5 issue #195 — MirJzSON gate compile-pass fixture for the
// nullable `Option<MirJzSON>` shape.
//
// The `#[mirjzson(justification = "...")]` attribute applies to the
// outer `Option<MirJzSON>` field just as it does to the bare
// `MirJzSON` shape — last-segment matching strips one `Option<…>`
// layer before the type check.

use djogi::prelude::*;

#[model(table = "phase85_mirjzson_optional_payloads")]
#[derive(Debug, Clone)]
pub struct OptionalPayload {
    pub source: String,
    #[mirjzson(justification = "schema lives in the downstream consumer service")]
    pub payload: Option<MirJzSON>,
}

fn main() {}
