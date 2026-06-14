use djogi::prelude::*;

#[model(table = "vsq_at_rest_codec_absent")]
#[derive(Debug, Clone)]
pub struct SecretBox {
    #[field(
        expose(public),
        protected(
            sensitivity = "secret",
            rationale = "at-rest codec field must not become a VisageColumn accessor",
            codec = "aes256_gcm_v1"
        )
    )]
    pub secret_token: String,
}

fn _at_rest_codec_accessor_absent() {
    let _ = SecretBoxPublic::secret_token();
}

fn main() {}
