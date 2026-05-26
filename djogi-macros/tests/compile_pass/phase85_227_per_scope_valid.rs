//! GH #227 Cluster A — valid `per_scope` usage on an exposed scalar field.
//!
//! Uses a single-segment imported codec path (`MaskString`) so the emitted
//! inventory/error metadata route through runtime type identity rather than the
//! macro's path re-stringification.
use djogi::prelude::*;
use djogi::presentation::builtins::MaskString;

#[model(table = "phase85_227_per_scope_valid")]
#[derive(Debug, Clone)]
pub struct User {
    #[field(
        expose(public, admin),
        protected(
            sensitivity = "pii",
            rationale = "email is masked in public only",
            per_scope = {
                public = {
                    presentation_codec = MaskString
                }
            }
        )
    )]
    pub email: String,

    #[field(expose(public, admin))]
    pub display_name: String,
}

fn main() {
    let _public = |user: &User| -> UserPublic { UserPublic::from(user) };
    let _admin = |user: &User| -> UserAdmin { UserAdmin::from(user) };
}
