use djogi::prelude::*;

#[model(table = "eq_all_mismatch_posts")]
#[derive(Debug, Clone)]
pub struct Post {
    #[field(expose(public))]
    pub author_id: i64,
}

#[model(table = "eq_all_mismatch_authors")]
#[derive(Debug, Clone)]
pub struct Author {
    #[field(expose(public))]
    pub tier: String,
}

fn main() {
    let bad = AuthorPublic::filter(|a| a.tier().eq("gold".to_string()))
        .selecting(AuthorPublic::tier())
        .unwrap();
    let _ = PostPublic::filter(|f| f.author_id().eq_all(bad));
}
