//! GH #227 — duplicate custom scope in `visage_scopes(...)` is rejected.
use djogi::prelude::*;

#[model(
    table = "phase85_227_duplicate_visage_scope",
    visage_scopes(support = Support, support = SupportCopy)
)]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public, support),
        protected(
            sensitivity = "pii",
            rationale = "for duplicate-scope rejection coverage",
            per_scope = {
                public = {
                    presentation_codec = djogi::presentation::builtins::MaskString
                }
            }
        )
    )]
    pub email: String,
}

fn main() {}
