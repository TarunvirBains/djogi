// Phase 7.5 T3 rule (a) — `sensitivity = "none"` is the explicit
// "ordinary field" assertion and cannot coexist with any other
// protected-field knob (rationale / redaction / codec / retention).
// Either the attribute disappears entirely or `sensitivity` rises.
//
// This fixture pins the rejection so a future macro refactor that
// silently widens what `sensitivity = "none"` accepts fails loudly
// in trybuild rather than landing as a behavioural drift.
use djogi::prelude::*;

#[model(table = "users")]
#[derive(Debug, Clone)]
pub struct User {
    #[field(protected(sensitivity = "none", redaction = "mask"))]
    pub note: String,
}

fn main() {}
