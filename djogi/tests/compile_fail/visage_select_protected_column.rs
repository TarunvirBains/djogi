//! A presentation-codec protected visage field is real data on the visage,
//! but it gets no `AuthorPublic::email()` accessor.
use djogi::prelude::*;
use djogi::presentation::builtins::MaskString;

#[model(table = "vsq_protected_authors")]
#[derive(Debug, Clone)]
pub struct Author {
 #[field(expose(public))]
 pub tier: String,

 #[field(
  expose(public),
  protected(
   sensitivity = "pii",
   rationale = "email is masked in public",
   per_scope = {
    public = {
     presentation_codec = MaskString
    }
   }
  )
 )]
 pub email: String,
}

fn main() {
 let author = Author {
  tier: "gold".to_string(),
  email: "ada@example.test".to_string(),
 ..Default::default()
 };
 let author_public: AuthorPublic = (&author).into();
 let _ = &author_public.email;

 let _ = AuthorPublic::email();
}
