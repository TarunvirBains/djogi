// Phase 7-Zero v3 T3 — raw-identifier column references resolve.
//
// Field names that collide with Rust keywords must be declared via the
// `r#` escape. The model-level index grammar accepts the same `r#ident`
// spelling and normalises it via `IdentExt::unraw` before the lookup
// against declared columns runs — so `fields = [r#yield]` matches
// `pub r#yield: String` on the struct.
use djogi::prelude::*;

#[model(table = "audit_where", indexes(
    index(fields = [r#yield]),
    index(fields = [(col = r#async, order = desc)]),
    index(fields = [r#yield], include = [r#async]),
))]
#[derive(Debug, Clone)]
pub struct AuditWhere {
    pub r#yield: String,
    pub r#async: String,
}

fn main() {}
