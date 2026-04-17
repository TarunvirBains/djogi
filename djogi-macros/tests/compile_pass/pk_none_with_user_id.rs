// Under `pk = "none"` the macro does not inject an `id` field, so the user
// is free to declare their own. This test verifies:
// - the user's `id` field survives (the name is NOT reserved under pk="none")
// - the generated Default impl includes the user's `id` via Default::default()
// - struct-update syntax still works
use djogi::prelude::*;

#[model(table = "custom_pk", pk = "none")]
#[derive(Debug, Clone)]
struct Custom {
    pub id: String,
    pub label: String,
}

fn _check_user_id_field(c: &Custom) {
    // User's own id type, not HeerId.
    let _: &String = &c.id;
    let _: &DateTime = &c.created_at;
    let _: &DateTime = &c.updated_at;
    let _: &str = &c.label;
}

fn _check_default_includes_user_id() {
    let c = Custom::default();
    // Default::default() on String gives "", so the user's id initializes cleanly.
    assert_eq!(c.id, String::default());
}

fn _check_struct_update() {
    let _c = Custom {
        id: "user-supplied".to_string(),
        label: "hello".to_string(),
        ..Custom::default()
    };
}

fn main() {}
