use djogi::prelude::*;
use serde_json::Value;

// Note: the is_jsonb_type and is_geography_type last-segment detection is
// fully covered by unit tests in djogi-macros/src/model/descriptor.rs:tests,
// which verify bare Jsonb<T>, qualified djogi::Jsonb<T>, and Option<Jsonb<T>> forms.
// These simple test cases demonstrate the explicit method and bare-form behavior.

// Test all valid index methods
#[model(table = "test_btree")]
#[derive(Debug, Clone)]
pub struct WithBTree {
    #[field(index = "btree")]
    pub name: String,
}

// Phase 7-Zero v3 T2 Q4: `#[field(index = "gin")]` is type-gated — the
// field type must be one of `Jsonb<T>`, `Vec<T>`, or `tsvector`. An
// earlier revision of this fixture put gin on a `String` column, which
// the new field-coherence validation rejects. `Jsonb<Value>` exercises
// the canonical accepted form.
#[model(table = "test_gin")]
#[derive(Debug, Clone)]
pub struct WithGin {
    #[field(index = "gin")]
    pub data: Jsonb<Value>,
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
