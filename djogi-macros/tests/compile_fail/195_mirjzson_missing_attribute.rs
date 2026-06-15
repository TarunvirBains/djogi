// MirJzSON gate compile-fail fixture.
//
// A `MirJzSON` field MUST carry `#[mirjzson(justification = "...")]`;
// declaring one without the attribute is rejected at expand time with a
// span-precise diagnostic pointing at the field.

use djogi::prelude::*;

#[model(table = "phase85_mirjzson_missing_attribute")]
#[derive(Debug, Clone)]
pub struct AuditLog {
 pub source: String,
 pub payload: MirJzSON,
}

fn main() {}
