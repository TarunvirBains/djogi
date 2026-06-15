//! A derived visage field is real data on the visage, but it gets no
//! `AuthorPublic::display_label()` accessor.
use djogi::prelude::*;

#[model(table = "vsq_der_authors")]
#[derive(Model, Debug, Clone, PartialEq)]
#[derived(
 name = display_label,
 ty = String,
 scopes = [public],
 sql = "upper(tier)",
 rust = "model.tier.to_uppercase()",
 doc = " Uppercased tier label - a derived projection, not a column.",
)]
pub struct Author {
 #[field(expose(public))]
 pub tier: String,
}

fn main() {
 let author = Author {
  tier: "gold".to_string(),
 ..Default::default()
 };
 let author_public: AuthorPublic = (&author).into();
 let _ = &author_public.display_label;

 let _ = AuthorPublic::display_label();
}
