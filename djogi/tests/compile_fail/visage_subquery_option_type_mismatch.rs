use djogi::prelude::*;

#[model(table = "vsq_opt_mismatch_users")]
#[derive(Debug, Clone)]
pub struct User {
    pub maybe_score: Option<i64>,
}

#[model(table = "vsq_opt_mismatch_scores")]
#[derive(Debug, Clone)]
pub struct ScoreLog {
    #[field(expose(public))]
    pub score: i32,
}

fn main() {
    let wrong_subquery = ScoreLogPublic::filter(|s| s.score().gte(0_i32))
        .selecting(ScoreLogPublic::score())
        .unwrap();

    let _ = User::objects().filter(|f| f.maybe_score().in_visage(wrong_subquery));
}
