// Phase 7-Zero-2 T13 fixup — `#[model(table = "...")]` values flow
// through `Model::table_name()` into raw SQL emission (e.g.
// `OuterRef::as_qualified_expr` → `<table>.<col>`, every `FROM <table>`
// emission, etc.). Without parse-time validation, a hostile table name
// could smuggle SQL metacharacters into the rendered output.
//
// This fixture pins the macro's rejection of a non-plain-identifier
// table name. The rejection is span-precise (points at the offending
// string literal) and reuses `crate::ident::check_one`, the same
// validator that already screens user-declared field column names.

use djogi::prelude::*;

#[model(table = "users; DROP TABLE x; --")]
#[derive(Debug, Clone)]
pub struct User {
    pub display_name: String,
}

fn main() {}
