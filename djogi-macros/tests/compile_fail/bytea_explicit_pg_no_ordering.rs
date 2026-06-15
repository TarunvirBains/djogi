// djogi#372 — ordering is a non-goal for BYTEA. Even via the explicit
// PostgreSQL predicate surface, `gt`/`gte`/`lt`/`lte`/`between` must NOT be
// callable on a BYTEA field. This proves the explicit-PG ordering hole that
// `Vec<u8>: IntoFilterValue` opened is closed: `Vec<u8>` does not implement
// `ExplicitPgOrderable`, so the five ordering methods on
// `ExplicitPgPredicateField<M, Vec<u8>>` do not resolve.
use djogi::prelude::*;

#[model(table = "blobs")]
#[derive(Debug, Clone)]
pub struct Blob {
 pub payload: Vec<u8>,
 pub label: String,
}

fn _no_bytea_ordering() {
 let _ = Blob::objects().filter(|f| f.payload().explicit_pg_predicate().gt(vec![1, 2]));
}

fn main() {}
