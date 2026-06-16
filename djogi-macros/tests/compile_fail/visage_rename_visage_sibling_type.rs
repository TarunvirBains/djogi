//! A custom visage name may not equal another scope's visage-keyed sibling
//! type (`{OtherVisage}Fields` / `{OtherVisage}Filter`). Renaming `public`
//! to `UserAdminFields` collides with the `Fields` type emitted for the
//! un-renamed `admin` scope's visage (`UserAdmin` → `UserAdminFields`).
use djogi::prelude::*;

#[model(table = "visage_rename_visage_sibling_type_users", visage_names(public = UserAdminFields))]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public, admin))]
    pub display_name: String,
}

fn main() {}
