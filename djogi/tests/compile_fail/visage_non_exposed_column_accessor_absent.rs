use djogi::prelude::*;

#[model(table = "vsq_non_exposed_authors")]
#[derive(Debug, Clone)]
pub struct Author {
    #[field(expose(public))]
    pub tier: String,
    pub password_hash: String,
}

fn _non_exposed_accessor_absent() {
    let _ = AuthorPublic::password_hash();
}

fn main() {}
