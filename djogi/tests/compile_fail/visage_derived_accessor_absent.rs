use djogi::prelude::*;

#[model(table = "vsq_derived_accessor_absent")]
#[derive(Model, Debug, Clone, PartialEq)]
#[derived(
    name   = display_label,
    ty     = String,
    scopes = [public],
    sql    = "name || '!'",
    rust   = "format!(\"{}!\", model.name)",
    doc    = " Derived display label.",
)]
pub struct Item {
    #[field(expose(public))]
    pub name: String,
}

fn _derived_accessor_absent() {
    let _ = ItemPublic::display_label();
}

fn main() {}
