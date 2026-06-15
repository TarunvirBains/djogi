// User field named after a fully-reserved Postgres keyword. The macro
// emits `COLUMN_LIST` containing "order" and the unquoted SQL
// `SELECT id, created_at, updated_at, order FROM...` is a syntax
// error. The macro-time validator rejects this at the user's field
// span instead of failing at SQL emission time.

use djogi::prelude::*;

#[model(table = "trades_reskw")]
#[derive(Debug, Clone)]
pub struct Trade {
 pub amount: i64,
 pub order: String,
}

fn main() {}
