// An unknown key in `#[field(...)]` must be rejected with a span-carrying
// error naming the offending attribute. Darling's FromField derive supplies
// this automatically; this fixture pins the behaviour so future edits to
// the FieldAttrs parser cannot regress span quality.
use djogi::prelude::*;

#[model(table = "posts")]
#[derive(Debug, Clone)]
pub struct Post {
 #[field(nonexistent_attr = 42)]
 pub title: String,
}

fn main() {}
