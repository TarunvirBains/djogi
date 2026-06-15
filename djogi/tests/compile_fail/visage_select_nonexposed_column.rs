//! A non-exposed column has no `AuthorPublic::password_hash()` accessor.
use djogi::prelude::*;

#[model(table = "vsq_ne_authors")]
#[derive(Debug, Clone)]
pub struct Author {
 #[field(expose(public))]
 pub tier: String,

 pub password_hash: String,
}

fn main() {
 let _ = AuthorPublic::password_hash();
}
