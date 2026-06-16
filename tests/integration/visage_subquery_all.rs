use djogi::prelude::*;

#[model(table = "c446_all_authors")]
#[derive(Debug, Clone)]
pub struct Author {
    #[field(expose(public))]
    pub score: i64,
}

#[model(table = "c446_all_posts")]
#[derive(Debug, Clone)]
pub struct Post {
    #[field(expose(public))]
    pub rating: i64,
    pub title: String,
}

fn mk_author(score: i64) -> Author {
    Author {
        id: <HeerIdRecencyBiased as PrimaryKey>::sentinel(),
        created_at: time::OffsetDateTime::now_utc(),
        updated_at: time::OffsetDateTime::now_utc(),
        score,
    }
}

fn mk_post(rating: i64, title: &str) -> Post {
    Post {
        id: <HeerIdRecencyBiased as PrimaryKey>::sentinel(),
        created_at: time::OffsetDateTime::now_utc(),
        updated_at: time::OffsetDateTime::now_utc(),
        rating,
        title: title.to_string(),
    }
}

// `rating = ALL (subquery)` is TRUE for every post when the subquery is empty
// (vacuous truth — standard Postgres ALL semantics). With zero authors matching
// the inner filter, every post row passes.
#[djogi::djogi_test(sync_models = [Author, Post])]
async fn eq_all_empty_subquery_is_vacuously_true(mut ctx: DjogiContext) {
    Post::create(&mut ctx, mk_post(10, "p10"))
        .await
        .expect("create p10");
    Post::create(&mut ctx, mk_post(20, "p20"))
        .await
        .expect("create p20");
    // No authors at all → the inner SELECT returns zero rows.

    let empty_scores = AuthorPublic::filter(|a| a.score().gt(1_000_000_i64))
        .selecting(AuthorPublic::score())
        .expect("no subquery modifiers");
    let rows = Post::objects()
        .filter(|f| f.rating().eq_all(empty_scores))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch rows through = ALL empty subquery");

    // Vacuous truth: both posts pass.
    assert_eq!(rows.len(), 2);
}

// `rating = ALL (subquery)` is TRUE only for the post whose rating equals every
// row of a single-valued subquery, and FALSE for posts that differ.
#[djogi::djogi_test(sync_models = [Author, Post])]
async fn eq_all_single_value_matches_only_equal_rows(mut ctx: DjogiContext) {
    Author::create(&mut ctx, mk_author(42))
        .await
        .expect("create author 42");

    Post::create(&mut ctx, mk_post(42, "match"))
        .await
        .expect("create matching post");
    Post::create(&mut ctx, mk_post(7, "nomatch"))
        .await
        .expect("create non-matching post");

    // Subquery yields exactly one value: 42.
    let only_42 = AuthorPublic::filter(|a| a.score().eq(42_i64))
        .selecting(AuthorPublic::score())
        .expect("no subquery modifiers");
    let rows = Post::objects()
        .filter(|f| f.rating().eq_all(only_42))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch rows through = ALL single-value subquery");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "match");
}

// `rating > ALL (subquery)` closes the ordering-family live-coverage gap:
// true only for posts strictly greater than every subquery row.
#[djogi::djogi_test(sync_models = [Author, Post])]
async fn gt_all_matches_rows_above_every_subquery_row(mut ctx: DjogiContext) {
    Author::create(&mut ctx, mk_author(5))
        .await
        .expect("create author 5");
    Author::create(&mut ctx, mk_author(8))
        .await
        .expect("create author 8");

    Post::create(&mut ctx, mk_post(10, "above"))
        .await
        .expect("create above post");
    Post::create(&mut ctx, mk_post(6, "between"))
        .await
        .expect("create between post");

    let all_scores = AuthorPublic::filter(|a| a.score().gte(i64::MIN))
        .selecting(AuthorPublic::score())
        .expect("no subquery modifiers");
    let rows = Post::objects()
        .filter(|f| f.rating().gt_all(all_scores))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch rows through > ALL subquery");

    // Only rating=10 is > both 5 and 8; rating=6 is not (6 < 8).
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "above");
}
