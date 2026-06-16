//! Swapping two scopes' names (each custom name is the other scope's
//! canonical `{Model}{Scope}` name) is a collision: the canonical aliases
//! would duplicate and cross-reference each other.
use djogi::prelude::*;

#[model(
    table = "visage_rename_canonical_swap_users",
    visage_names(public = UserAdmin, admin = UserPublic)
)]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public, admin))]
    pub display_name: String,
}

fn main() {}
