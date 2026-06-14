use djogi::prelude::*;

#[model(table = "vsq_forge_ctor_authors")]
#[derive(Debug, Clone)]
pub struct Author {
    #[field(expose(public))]
    pub tier: String,
}

fn _forge_token() {
    let _t = ::djogi::__private::visage_column_seal::VisageColumnToken::__new();
}

fn main() {}
