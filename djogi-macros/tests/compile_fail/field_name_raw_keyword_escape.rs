// Rust raw-identifier escape (`r#select`) is a valid Rust ident but
// stringifies to the reserved Postgres keyword `select`. Without the
// macro-time column-name validator, this silently emits invalid SQL
// (`SELECT id, created_at, updated_at, select FROM ...`). The
// validator catches it at the user's field span.

use djogi::prelude::*;

#[model(table = "trades_raw_kw")]
#[derive(Debug, Clone)]
pub struct Trade {
    pub amount: i64,
    pub r#select: String,
}

fn main() {}
