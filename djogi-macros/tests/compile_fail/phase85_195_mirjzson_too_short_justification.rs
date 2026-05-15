// Phase 8.5 issue #195 — MirJzSON gate compile-fail fixture.
//
// Justifications below the 12-byte minimum (after trim) are rejected.
// The bar exists to weed out single-word non-answers that slip past the
// placeholder denylist (e.g. "hmm.", "schema?").

use djogi::prelude::*;

#[model(table = "phase85_mirjzson_too_short_justification")]
#[derive(Debug, Clone)]
pub struct AuditLog {
    pub source: String,
    #[mirjzson(justification = "schema?")]
    pub payload: MirJzSON,
}

fn main() {}
