//! Renaming one scope's visage to another scope's canonical
//! `{Model}{Scope}` name is a collision — the macro still emits that name
//! as the other scope's canonical alias, so reusing it is a duplicate
//! definition.
use djogi::prelude::*;

#[model(table = "visage_rename_shadows_canonical_users", visage_names(public = UserAdmin))]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public, admin))]
    pub display_name: String,
}

fn main() {}
