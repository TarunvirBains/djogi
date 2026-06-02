// `#[model(tree_edge = "...")]` value
// must name an existing field on the struct.
//
// The macro validates the named column at expansion time: if the named
// field is absent from the user's struct, the macro raises a span-precise
// compile error pointing at the offending string literal, with the message
// instructing the caller to declare the FK first.
//
// `fn main() {}` is required for lihaaf compile-fail fixtures (see
// lihaaf compile-fixture contract) — the .stderr would otherwise carry an
// E0601 noise line for the missing main.

use djogi::prelude::*;

#[model(table = "phase8_invalid_field_nodes", tree_edge = "nonexistent_field")]
#[derive(Debug, Clone)]
pub struct InvalidEdgeNode {
    pub label: String,
}

fn main() {}
