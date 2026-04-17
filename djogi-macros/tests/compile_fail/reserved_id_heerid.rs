// Under the default `pk = "heerid"` strategy, the macro injects `id` as
// HeerId, so a user `id` field collides and must be rejected with a targeted
// macro diagnostic that points at the offending field.
use djogi::prelude::*;

#[model(table = "posts")]
struct Bad {
    pub id: String,
    pub title: String,
}

fn main() {}
