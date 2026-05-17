use djogi::prelude::*;

#[model(table = "widgets", storage_params = "fillfactor=70); DROP TABLE x; --")]
#[derive(Debug, Clone)]
pub struct Widget {
    pub name: String,
}

fn main() {}
