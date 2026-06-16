//! A custom visage name may not collide with a model-keyed sibling type
//! the derive emits (`{Model}Fields`, `{Model}Filter`, `{Model}Related`,
//! …). Renaming `public` to `UserFilter` collides with the `{Model}Filter`
//! type emitted for `User`.
use djogi::prelude::*;

#[model(table = "visage_rename_sibling_type_users", visage_names(public = UserFilter))]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public))]
    pub display_name: String,
}

fn main() {}
