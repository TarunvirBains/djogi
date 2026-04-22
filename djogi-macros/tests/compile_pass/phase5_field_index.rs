use djogi::prelude::*;

// Test all valid index methods
#[model(table = "test_btree")]
#[derive(Debug, Clone)]
pub struct WithBTree {
    #[field(index = "btree")]
    pub name: String,
}

#[model(table = "test_gin")]
#[derive(Debug, Clone)]
pub struct WithGin {
    #[field(index = "gin")]
    pub data: String,
}

#[model(table = "test_gist")]
#[derive(Debug, Clone)]
pub struct WithGist {
    #[field(index = "gist")]
    pub location: String,
}

#[model(table = "test_brin")]
#[derive(Debug, Clone)]
pub struct WithBrin {
    #[field(index = "brin")]
    pub timestamp_data: String,
}

#[model(table = "test_hash")]
#[derive(Debug, Clone)]
pub struct WithHash {
    #[field(index = "hash")]
    pub unique_code: String,
}

#[model(table = "test_spgist")]
#[derive(Debug, Clone)]
pub struct WithSpgist {
    #[field(index = "spgist")]
    pub spatial_data: String,
}

// Test bare #[field(index)] with auto-defaults

// Jsonb field with bare index → defaults to Gin
// Note: Using String instead of full Jsonb<T> type to avoid serde_json dependency in test
#[model(table = "test_bare_jsonb")]
#[derive(Debug, Clone)]
pub struct BareJsonb {
    #[field(index)]
    pub metadata: String,  // In real use, would be Jsonb<T>
}

// String field with bare index → defaults to BTree
#[model(table = "test_bare_string")]
#[derive(Debug, Clone)]
pub struct BareString {
    #[field(index)]
    pub title: String,
}

// Integer field with bare index → defaults to BTree
#[model(table = "test_bare_integer")]
#[derive(Debug, Clone)]
pub struct BareInteger {
    #[field(index)]
    pub count: i32,
}

fn main() {}
