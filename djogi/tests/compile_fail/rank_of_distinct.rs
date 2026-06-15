//! Compile-fail fixture for #89: hypothetical-set aggregates should not
//! expose `.distinct()`. Mirrors `percentile_cont_distinct.rs` for the
//! `HypotheticalSetAgg` modifier family.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_rank_of_distinct", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct RankOfDistinctModel {
 pub salary: i64,
}

fn main() {
 let _ = RankOfDistinctModel::objects().annotate(|f| f.salary().rank_of(7_500).distinct());
}
