// `djogi::primary_key!` rejects attribute keys that
// are not one of {sql_type, default_sql, bulk_sql, generate}. Misspelled
// keys should surface a diagnostic rather than silently ignore the line.

djogi::primary_key! {
    pub struct Bad(i64);
    sql_type = "BIGINT";
    default_sql = "0";
    flibber = "x";
}

fn main() {}
