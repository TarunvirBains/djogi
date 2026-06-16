//! Two scopes renamed to the SAME custom name is a compile error — each
//! scope's visage must have a unique type name.
use djogi::prelude::*;

#[model(
    table = "visage_rename_collision_users",
    visage_names(public = SharedName, admin = SharedName)
)]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public, admin))]
    pub display_name: String,
}

fn main() {}
