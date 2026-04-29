// Malformed `protected` shape: bare path form.
//
// `#[field(protected)]` is not valid syntax for protected-field
// metadata; the only valid form is `protected(sensitivity = "...",
// ...)`. An earlier pass silently dropped this shape because the inner
// match arm only recognised `Meta::List`, leaving the adopter's intent
// to vanish without diagnostic. The macro must reject it explicitly so
// the operator hears why nothing applied.
use djogi::prelude::*;

#[model(table = "users")]
#[derive(Debug, Clone)]
pub struct User {
    #[field(protected)]
    pub note: String,
}

fn main() {}
