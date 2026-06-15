// §5 positive case: covering index with INCLUDE columns.
use djogi::prelude::*;

#[model(table = "events", indexes(
 index(fields = [created_at], include = [status, priority]),
))]
#[derive(Debug, Clone)]
pub struct Event {
 pub status: String,
 pub priority: i32,
}

fn main() {}
