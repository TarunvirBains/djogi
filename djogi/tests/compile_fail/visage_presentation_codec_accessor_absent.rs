use djogi::prelude::*;
use djogi::presentation::builtins::MaskString;

#[model(table = "vsq_presentation_codec_absent")]
#[derive(Debug, Clone)]
pub struct User {
    #[field(
        expose(public),
        protected(
            sensitivity = "pii",
            rationale = "presentation codec accessor must not project masked field",
            per_scope = {
                public = {
                    presentation_codec = MaskString
                }
            }
        )
    )]
    pub email: String,
}

fn _presentation_codec_accessor_absent() {
    let _ = UserPublic::email();
}

fn main() {}
