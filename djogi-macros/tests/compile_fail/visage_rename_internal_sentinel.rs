//! `visage_names(internal = ...)` and `visage_names(none = ...)` are both
//! rejected — the `internal` / `none` sentinels generate no visage struct,
//! so there is nothing to rename. One model per sentinel proves both arms
//! of the `matches!("none" | "internal")` rejection in
//! `parse_visage_names_list`.
use djogi::prelude::*;

#[model(table = "visage_rename_internal_sentinel_users", visage_names(internal = Foo))]
#[derive(Debug, Clone)]
pub struct UserInternal {
    #[field(expose(public))]
    pub display_name: String,
}

#[model(table = "visage_rename_none_sentinel_users", visage_names(none = Bar))]
#[derive(Debug, Clone)]
pub struct UserNone {
    #[field(expose(public))]
    pub display_name: String,
}

fn main() {}
