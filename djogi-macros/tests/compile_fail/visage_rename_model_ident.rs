//! A custom visage name must not equal the source model's type name.
use djogi::prelude::*;

#[model(table = "visage_rename_model_ident_users", visage_names(public = User))]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public))]
    pub display_name: String,
}

fn main() {}
