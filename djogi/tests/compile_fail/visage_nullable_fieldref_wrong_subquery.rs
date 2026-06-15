use djogi::prelude::*;

#[model(table = "vsq_null_fr_scores")]
#[derive(Debug, Clone)]
pub struct NullScore {
    #[field(expose(public))]
    pub maybe_score: Option<i64>,
}

#[model(table = "vsq_null_fr_tiers")]
#[derive(Debug, Clone)]
pub struct Tier {
    #[field(expose(public))]
    pub level: i32,
}

fn main() {
    let wrong_subquery = TierPublic::filter(|t| t.level().gte(0_i32))
        .selecting(TierPublic::level())
        .unwrap();

    let _ = NullScorePublic::filter(|f| f.maybe_score().in_visage(wrong_subquery));
}
