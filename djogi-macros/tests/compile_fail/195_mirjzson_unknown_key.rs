// MirJzSON gate compile-fail fixture.
//
// The only accepted key inside `#[mirjzson(...)]` is `justification`.
// Common misspellings (`reason`, `explanation`, `because`, etc.) are
// rejected with an "unsupported key" diagnostic.

use djogi::prelude::*;

#[model(table = "phase85_mirjzson_unknown_key")]
#[derive(Debug, Clone)]
pub struct AuditLog {
 pub source: String,
 #[mirjzson(reason = "payload is externally owned by partner API")]
 pub payload: MirJzSON,
}

fn main() {}
