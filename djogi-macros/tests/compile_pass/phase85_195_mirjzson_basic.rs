// Phase 8.5 issue #195 — MirJzSON gate compile-pass fixture.
//
// A required `MirJzSON` field with a specific, non-placeholder
// `#[mirjzson(justification = "...")]` annotation accepts cleanly. The
// macro consumes the attribute and emits a struct rustc can compile
// without an `unknown attribute mirjzson` error.
//
// `MirJzSON` is intentionally NOT `Default` (whole-value JSON values do
// not have a meaningful zero — `JSahibON::Null` would be a foot-gun),
// so the enclosing model opts out of the `Default` impl via
// `#[model(no_default)]`. Adopters who need `..Model::default()`
// struct-update syntax must initialise the `MirJzSON` field
// explicitly each time.

use djogi::prelude::*;

#[model(table = "phase85_mirjzson_audit_logs", no_default)]
#[derive(Debug, Clone)]
pub struct AuditLog {
    pub source: String,
    #[mirjzson(justification = "payload is externally owned by partner API")]
    pub payload: MirJzSON,
}

fn main() {}
