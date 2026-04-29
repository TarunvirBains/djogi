// Malformed `protected` shape: name-value form.
//
// `#[field(protected = "pii")]` is not valid syntax for protected-field
// metadata; the only valid form is `protected(sensitivity = "...",
// ...)`. An earlier pass silently dropped this shape because the inner
// match arm only recognised `Meta::List`, so an adopter who reached
// for the simpler-looking name-value form got their annotation
// accepted-then-discarded. The macro must reject it explicitly.
use djogi::prelude::*;

#[model(table = "users")]
#[derive(Debug, Clone)]
pub struct User {
    #[field(protected = "pii")]
    pub note: String,
}

fn main() {}
