//! Compile-fail fixture for #89: hypothetical-set aggregates should not
//! expose `.over(...)`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_rank_of_over", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct RankOfOverModel {
 pub salary: i64,
}

fn main() {
 let _ = RankOfOverModel::objects().annotate(|f| f.salary().rank_of(7_500).over(|w| w));
}
