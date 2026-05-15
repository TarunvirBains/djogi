// Phase 8.5 issue #195 — MirJzSON gate compile-fail fixture.
//
// `justification = ...` requires a string literal — integer / boolean /
// path values are rejected with a "must be a string literal"
// diagnostic.

use djogi::prelude::*;

#[model(table = "phase85_mirjzson_non_string_justification")]
#[derive(Debug, Clone)]
pub struct AuditLog {
    pub source: String,
    #[mirjzson(justification = 42)]
    pub payload: MirJzSON,
}

fn main() {}
