// field-level `nulls_not_distinct` is out of scope.
//
// Per v2 decision #8, partial-uniqueness and `NULLS NOT DISTINCT` live
// on the model-level `#[model(indexes(unique(...)))]` grammar, not the
// field-level shorthand. The macro rejects field-level
// `nulls_not_distinct = true` with a compile error pointing the author
// at the model-level syntax.
use djogi::prelude::*;

#[model(table = "posts")]
#[derive(Debug, Clone)]
pub struct Post {
    #[field(unique, nulls_not_distinct = true)]
    pub slug: Option<String>,
}

fn main() {}
