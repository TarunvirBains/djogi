// MirJzSON gate compile-fail fixture.
//
// Placeholder values such as `TODO`, `TBD`, `FIXME`, `?`, `none`, and
// similar are rejected. The denylist is ASCII case-insensitive and the
// matcher applies to the trimmed value; sentences that CONTAIN one of
// these tokens still pass — only standalone placeholders fail.

use djogi::prelude::*;

#[model(table = "phase85_mirjzson_vague_justification")]
#[derive(Debug, Clone)]
pub struct AuditLog {
    pub source: String,
    #[mirjzson(justification = "TODO")]
    pub payload: MirJzSON,
}

fn main() {}
