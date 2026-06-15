// MirJzSON gate compile-fail fixture.
//
// A field carries one `#[mirjzson(...)]` attribute, not two. A
// second copy on the same field is rejected with a "duplicate"
// diagnostic at the offending attribute's span.

use djogi::prelude::*;

#[model(table = "phase85_mirjzson_duplicate_attribute")]
#[derive(Debug, Clone)]
pub struct AuditLog {
 pub source: String,
 #[mirjzson(justification = "payload is externally owned by partner API")]
 #[mirjzson(justification = "second annotation should be rejected as duplicate")]
 pub payload: MirJzSON,
}

fn main() {}
