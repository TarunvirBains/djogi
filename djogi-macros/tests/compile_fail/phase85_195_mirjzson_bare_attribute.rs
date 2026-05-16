// Phase 8.5 issue #195 — MirJzSON gate compile-fail fixture.
//
// `#[mirjzson]` (the bare path form without an argument list) is
// rejected. The attribute exists solely to record a justification —
// there is no "just declare it's gated, no reason needed" shortcut.

use djogi::prelude::*;

#[model(table = "phase85_mirjzson_bare_attribute")]
#[derive(Debug, Clone)]
pub struct AuditLog {
    pub source: String,
    #[mirjzson]
    pub payload: MirJzSON,
}

fn main() {}
