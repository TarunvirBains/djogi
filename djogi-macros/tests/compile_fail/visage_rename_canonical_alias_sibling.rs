//! `visage_names(...)` custom name that matches the canonical-alias Fields
//! sibling of another renamed scope is rejected at compile time.
//!
//! When `public` is renamed to `UserSummary`, the macro emits
//! `pub type UserPublicFields<RootModel = User> = UserSummaryFields<RootModel>;`
//! as a canonical-alias sibling. A different scope must not use `UserPublicFields`
//! as its custom name — this is caught with a span-precise diagnostic.
use djogi::prelude::*;

#[model(
    table = "visage_rename_canonical_alias_sibling_users",
    visage_names(public = UserSummary, admin = UserPublicFields)
)]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public, admin))]
    pub email: String,
}

fn main() {}
