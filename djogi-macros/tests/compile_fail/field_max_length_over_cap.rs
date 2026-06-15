// `#[field(max_length = 10_485_761)]` exceeds PostgreSQL's VARCHAR cap and
// must be rejected at macro-expansion time.
//
// Postgres caps `VARCHAR(N)` at 10_485_760, so one more is rejected with a
// compile error rather than leaking through to migration-time DDL.
use djogi::prelude::*;

#[model(table = "over_cap_text", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct OverCapText {
 #[field(max_length = 10485761)]
 pub title: String,
}

fn main() {}
