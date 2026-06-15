// §5 positive case: method + uniform opclass.
use djogi::prelude::*;
use serde_json::Value;

#[model(table = "profiles", indexes(
 index(fields = [payload], using = "gin", opclass = "jsonb_path_ops"),
))]
#[derive(Debug, Clone)]
pub struct Profile {
 pub payload: Jsonb<Value>,
}

fn main() {}
