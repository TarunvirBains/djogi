use djogi::prelude::*;

#[model(table = "brace_targets", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct BraceTarget {
    pub id_col: i32,
}

fn main() {
    let _t: ConflictTarget<BraceTarget> = ConflictTarget::Columns {
        columns: vec!["id_col"],
        inference_predicate: None,
    };
}
