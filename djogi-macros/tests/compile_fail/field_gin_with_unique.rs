// `#[field(unique, index = "gin")]` is rejected.
//
// PostgreSQL unique indexes are btree-only, so a non-btree
// `index = "gin"` combined with `unique` is ambiguous (it cannot mean
// "unique gin index" — that is impossible). The macro rejects this with
// a span-precise diagnostic that lists the three resolutions: use btree,
// drop unique, or split into a field-level `unique` + a model-level
// `index(..., using = "gin")`.
//
// Field type is `String` here — the unique+method rejection fires
// before the gin-type-gate that would otherwise reject gin-on-scalar,
// so the diagnostic surfaces from the unique rule rather than from the
// type rule.
use djogi::prelude::*;

#[model(table = "profiles")]
#[derive(Debug, Clone)]
pub struct Profile {
 #[field(index = "gin", unique)]
 pub payload: String,
}

fn main() {}
