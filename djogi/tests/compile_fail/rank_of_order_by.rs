//! Compile-fail fixture for #89: hypothetical-set aggregates should not
//! expose in-paren `.order_by(...)`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_rank_of_order_by", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct RankOfOrderByModel {
    pub salary: i64,
}

fn main() {
    let _ = RankOfOrderByModel::objects().annotate(|f| {
        f.salary().rank_of(7_500).order_by(f.salary().asc())
    });
}
