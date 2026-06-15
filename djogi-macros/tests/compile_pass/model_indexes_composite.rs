// §5 positive case: composite non-unique index.
use djogi::prelude::*;

#[model(table = "customers", indexes(
 index(fields = [last_name, first_name]),
))]
#[derive(Debug, Clone)]
pub struct Customer {
 pub first_name: String,
 pub last_name: String,
}

fn main() {}
