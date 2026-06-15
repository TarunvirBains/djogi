//! An at-rest codec field is real data on the visage, but it gets no
//! `AuthorPublic::secret_token()` accessor.
#![cfg(feature = "aes-codec")]

use djogi::prelude::*;

#[model(table = "vsq_atrest_authors")]
#[derive(Debug, Clone)]
pub struct Author {
 #[field(expose(public))]
 pub tier: String,

 #[field(
  expose(public),
  protected(
   sensitivity = "secret",
   rationale = "encrypted at rest",
   codec = "aes256_gcm_v1"
  )
 )]
 pub secret_token: String,
}

fn main() {
 let author = Author {
  tier: "gold".to_string(),
  secret_token: "super-secret".to_string(),
 ..Default::default()
 };
 let author_public: AuthorPublic = (&author).into();
 let _ = &author_public.secret_token;

 let _ = AuthorPublic::secret_token();
}
