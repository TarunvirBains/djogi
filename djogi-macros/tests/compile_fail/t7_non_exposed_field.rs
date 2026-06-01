//! non-exposed scalar field is ABSENT from the
//! `{Visage}Fields` accessor surface.
//!
//! `email` is declared `expose(self_view)` only — it does NOT appear in
//! the `public` scope. Referencing `UserPublicFields::email()` therefore
//! fails with rustc's "no function or associated item named …" — the
//! absence-by-construction contract T7 lands.
use djogi::prelude::*;

#[model(table = "users_t7_non_exposed_field")]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public))]
    pub display_name: String,
    #[field(expose(self_view))]
    pub email: String,
}

fn main() {
    // UserPublic does NOT expose email — accessor is not generated.
    let _bad = UserPublicFields::email();
}
