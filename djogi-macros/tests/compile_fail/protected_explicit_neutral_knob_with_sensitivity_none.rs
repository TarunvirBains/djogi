// Span-presence variant of the `sensitivity = "none"` rule. The user
// wrote `redaction = "none"` explicitly: the resulting `RedactionLit`
// value matches the neutral default, but the *presence* of the key
// alongside `sensitivity = "none"` is still a contradiction. The
// validator must discriminate by per-key presence (span-tracked), not
// by comparing final values to the neutral default — otherwise an
// adopter who explicitly wrote `redaction = "none"` would have their
// attribute silently accepted while a value of `redaction = "mask"`
// rejects.
//
// The redaction-with-mask fixture pins one shape of the
// `sensitivity = "none"` rule; this fixture pins the
// explicit-neutral-knob case so a regression to value-comparison fails
// loudly in lihaaf.
use djogi::prelude::*;

#[model(table = "users")]
#[derive(Debug, Clone)]
pub struct User {
 #[field(protected(sensitivity = "none", redaction = "none"))]
 pub note: String,
}

fn main() {}
