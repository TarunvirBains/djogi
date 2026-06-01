//! GH #227 — `protected(per_scope = { ... })` may only name
//! scopes that the field itself exposes.
//!
//! `email` is exposed only in `public`, so the `admin = { ... }` codec block
//! must be rejected at compile time instead of silently dangling.
use djogi::prelude::*;

#[model(table = "phase85_227_per_scope_unexposed_scope")]
#[derive(Debug, Clone)]
pub struct User {
    #[field(
        expose(public),
        protected(
            sensitivity = "pii",
            rationale = "email needs public masking",
            per_scope = {
                admin = {
                    presentation_codec = djogi::presentation::builtins::MaskString
                }
            }
        )
    )]
    pub email: String,
}

fn main() {}
