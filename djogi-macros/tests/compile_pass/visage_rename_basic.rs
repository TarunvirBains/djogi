//! A renamed visage is reachable under BOTH its custom name and the
//! canonical `{Model}{Scope}` alias.
//!
//! Renaming the public visage to `UserSummary` must still leave
//! `UserPublic` usable — that canonical alias is what keeps existing
//! relation embeddings and downstream references compiling unedited.
use djogi::prelude::*;

#[model(table = "visage_rename_basic_users", visage_names(public = UserSummary))]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public, admin))]
    pub display_name: String,
}

fn main() {
    // The renamed struct resolves under its custom name.
    fn assert_custom(_: &UserSummary) {}
    // The canonical alias still resolves to the same type.
    fn assert_alias(_: &UserPublic) {}

    // The two names denote the same type: construct under the custom name,
    // pass to a function typed on the canonical alias. The model is built
    // via `User::default()` — the corpus convention for fixtures that need
    // a model value (e.g. `required_fk_traversal.rs`, `derived_fields.rs`);
    // no fixture constructs the framework-injected `id`/`created_at`/
    // `updated_at` fields by name in a struct literal. The `#[derive(Model)]`
    // macro emits `Default` unless `no_default` is set, and the default PK
    // (`HeerIdRecencyBiased`) derives `Default`, so `User::default()`
    // resolves here.
    let u = User::default();
    let summary: UserSummary = UserSummary::from(&u);
    assert_custom(&summary);
    assert_alias(&summary); // compiles iff `UserPublic` aliases `UserSummary`

    // The un-renamed scope keeps its canonical name.
    let _admin: UserAdmin = UserAdmin::from(&u);
}
