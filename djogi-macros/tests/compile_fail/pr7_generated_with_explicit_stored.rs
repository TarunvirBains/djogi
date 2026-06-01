// `stored = ...` is not accepted alongside
// `generated = "<expr>"` in V1 syntax.
//
// Pg18 supports only STORED generated columns, so the macro hard-codes
// `stored: true` at lowering time and rejects the explicit knob with a
// dedicated diagnostic. The slot exists on the descriptor type for a
// future Pg19+ VIRTUAL variant but is intentionally inaccessible from
// the attribute syntax today — one less surface to maintain across
// review rounds.
use djogi::prelude::*;

#[model(table = "users", no_default)]
#[derive(Debug, Clone)]
pub struct User {
    pub email: String,
    #[field(generated = "LOWER(email)", stored = true)]
    pub email_lower: String,
}

fn main() {}
