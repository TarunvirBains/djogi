// Phase 7.5 T3 rule (a) — span-presence variant. The user wrote
// `redaction = "none"` explicitly: the resulting `RedactionLit` value
// matches the neutral default, but the *presence* of the key alongside
// `sensitivity = "none"` is still a contradiction. Rule (a) must
// discriminate by per-key presence (span-tracked), not by comparing
// final values to the neutral default — otherwise an adopter who
// explicitly wrote `redaction = "none"` would have their attribute
// silently accepted while a value of `redaction = "mask"` rejects.
//
// The earlier T3 fixture asserts the redaction-with-mask shape; this
// fixture pins the explicit-neutral-knob case so a regression to
// value-comparison fails loudly in trybuild.
use djogi::prelude::*;

#[model(table = "users")]
#[derive(Debug, Clone)]
pub struct User {
    #[field(protected(sensitivity = "none", redaction = "none"))]
    pub note: String,
}

fn main() {}
