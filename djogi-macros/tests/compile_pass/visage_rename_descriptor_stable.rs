//! Renaming a visage does not perturb the visage's query entry points.
//! The renamed visage and its canonical alias both expose working queryset
//! filter entry points (`{Visage}::filter`). Descriptor stability (`type_name`
//! is always the model struct ident, never the visage ident) is structurally
//! guaranteed by `djogi-macros/src/model/descriptor.rs` and not re-verified
//! here.
use djogi::prelude::*;

#[model(table = "visage_rename_descriptor_stable", visage_names(public = PublicUserCard))]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public, admin))]
    pub display_name: String,
}

fn main() {
    // The renamed visage exposes its filter entry point under the custom
    // name (the macro emits `{Visage}::filter` keyed on the visage ident).
    // We only need this to *type-check*, not run — no DB connection here.
    fn _uses_filter() {
        let _qs = PublicUserCard::filter(|f| f.display_name().eq("Ada".to_string()));
    }
    // The canonical alias also reaches the same queryset entry.
    fn _uses_alias_filter() {
        let _qs = UserPublic::filter(|f| f.display_name().eq("Ada".to_string()));
    }
}
