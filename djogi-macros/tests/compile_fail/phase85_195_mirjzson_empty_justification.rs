// Phase 8.5 issue #195 — MirJzSON gate compile-fail fixture.
//
// An empty `justification = ""` is rejected with a span pointing at the
// literal. The macro requires a specific, non-empty reason — the whole
// point of the gate is to record WHY the adopter reached for raw JSONB
// instead of `Jsonb<T>`.

use djogi::prelude::*;

#[model(table = "phase85_mirjzson_empty_justification")]
#[derive(Debug, Clone)]
pub struct AuditLog {
    pub source: String,
    #[mirjzson(justification = "")]
    pub payload: MirJzSON,
}

fn main() {}
