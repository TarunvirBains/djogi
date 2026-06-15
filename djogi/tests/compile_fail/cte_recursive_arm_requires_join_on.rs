//! `with_recursive` requires the recursive term to be a typed
//! `RecursiveArm<M>`, not an arbitrary queryset.

use djogi::prelude::*;

#[model(table = "cf_cte_node", pk = HeerId)]
#[derive(Debug, Clone)]
struct Node {
    parent_id: i64,
    label: String,
}

fn main() {
    let anchor = Node::objects();
    let _ = Node::objects().with_recursive("walk", anchor, Node::objects());
}
