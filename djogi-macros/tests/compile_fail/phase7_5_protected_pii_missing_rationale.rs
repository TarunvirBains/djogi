// `sensitivity` above `none` requires a
// non-empty `rationale`. The rationale is the audit trail's primary
// signal (e.g. citing GDPR Art. 6(1)(b) for a notification-delivery
// PII column); a missing rationale defeats the entire annotation.
use djogi::prelude::*;

#[model(table = "users")]
#[derive(Debug, Clone)]
pub struct User {
    #[field(protected(sensitivity = "pii"))]
    pub email: String,
}

fn main() {}
