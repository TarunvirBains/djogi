// `djogi::primary_key!` requires `default_sql = "..."`.
// Custom PK types always carry a column default so the `#[model]` DDL
// emitter has a deterministic clause to write; omitting it is a compile
// error at parse time.

djogi::primary_key! {
 pub struct Bad(i64);
 sql_type = "BIGINT";
}

fn main() {}
