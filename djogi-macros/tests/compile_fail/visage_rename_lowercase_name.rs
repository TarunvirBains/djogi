//! A custom visage name must start with an uppercase ASCII letter.
use djogi::prelude::*;

#[model(table = "visage_rename_lowercase_name_users", visage_names(public = userSummary))]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public))]
    pub display_name: String,
}

fn main() {}
