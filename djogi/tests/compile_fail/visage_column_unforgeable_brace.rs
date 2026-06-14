use djogi::prelude::*;
use djogi::query::VisageColumn;

#[model(table = "vsq_forge_brace_authors")]
#[derive(Debug, Clone)]
pub struct Author {
    #[field(expose(public))]
    pub tier: String,
    pub password_hash: String,
}

fn _forge_by_brace() {
    let _bad: VisageColumn<AuthorPublic, String> = VisageColumn {
        column: "password_hash",
    };
}

fn main() {}
