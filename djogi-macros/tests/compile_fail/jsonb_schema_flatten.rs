// #[serde(flatten)] on a JsonbSchema field must be rejected at compile time
// with a clear diagnostic — flattened keys cannot be addressed via static path.
use djogi::JsonbSchema;

#[derive(JsonbSchema, serde::Serialize, serde::Deserialize)]
pub struct Bad {
    #[serde(flatten)]
    pub extras: std::collections::HashMap<String, serde_json::Value>,
}

fn main() {}
