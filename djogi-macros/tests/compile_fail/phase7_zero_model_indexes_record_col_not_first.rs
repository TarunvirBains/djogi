// Phase 7-Zero v3 T3 — §5 grammar rejection: column record must open
// with `col = ident`. Any subsequent opclass/order/nulls key is
// optional, but "after col" is non-negotiable per the documented
// grammar ("any subset of the record fields after `col` is optional").
use djogi::prelude::*;

#[model(table = "users", indexes(
    index(fields = [(order = desc, col = email)]),
))]
#[derive(Debug, Clone)]
pub struct User {
    pub email: String,
}

fn main() {}
