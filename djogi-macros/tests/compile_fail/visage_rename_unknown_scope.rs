//! `visage_names(...)` naming a scope the model does not declare is a
//! compile error — the adopter likely typo'd a scope key.
use djogi::prelude::*;

#[model(table = "visage_rename_unknown_scope", visage_names(notascope = Foo))]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public))]
    pub display_name: String,
}

fn main() {}
