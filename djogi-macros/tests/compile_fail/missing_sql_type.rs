// `djogi::primary_key!` requires `sql_type = "..."`.
// Omitting it is a compile error at parse time, not a runtime failure.

djogi::primary_key! {
 pub struct Bad(i64);
 default_sql = "0";
}

fn main() {}
