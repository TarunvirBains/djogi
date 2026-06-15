// `#[field(index = "gin")]` accepted on Jsonb,
// Vec, and TsVector.
//
// GIN is valid on Postgres JSONB columns (via the `jsonb_ops` /
// `jsonb_path_ops` opclasses), on array-typed columns, and on FTS
// `tsvector` columns. All three accepted forms must compile without
// a type-coherence error.
use djogi::prelude::*;
use serde_json::Value;

#[model(table = "profiles_gin_jsonb")]
#[derive(Debug, Clone)]
pub struct ProfileJsonb {
 #[field(index = "gin")]
 pub traits: Jsonb<Value>,
}

#[model(table = "profiles_gin_vec")]
#[derive(Debug, Clone)]
pub struct ProfileVec {
 #[field(index = "gin")]
 pub tags: Vec<String>,
}

#[model(table = "profiles_gin_tsvector")]
#[derive(Debug, Clone)]
pub struct ProfileTsVector {
 pub source: String,
 #[field(index = "gin")]
 pub search: TsVector,
}

fn main() {}
