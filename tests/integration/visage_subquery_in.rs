use djogi::prelude::*;

#[model(table = "vsq_in_authors")]
#[derive(Debug, Clone)]
pub struct Author {
    #[field(expose(public))]
    pub tier: String,
}

#[model(table = "vsq_in_posts")]
#[derive(Debug, Clone)]
pub struct Post {
    pub author_id: HeerIdRecencyBiased,
    #[field(expose(public))]
    pub title: String,
}

fn mk_author(tier: &str) -> Author {
    Author {
        id: <HeerIdRecencyBiased as PrimaryKey>::sentinel(),
        created_at: time::OffsetDateTime::now_utc(),
        updated_at: time::OffsetDateTime::now_utc(),
        tier: tier.to_string(),
    }
}

fn mk_post(author_id: HeerIdRecencyBiased, title: &str) -> Post {
    Post {
        id: <HeerIdRecencyBiased as PrimaryKey>::sentinel(),
        created_at: time::OffsetDateTime::now_utc(),
        updated_at: time::OffsetDateTime::now_utc(),
        author_id,
        title: title.to_string(),
    }
}

#[djogi::djogi_test(sync_models = [Author, Post])]
async fn in_visage_returns_matching_rows(mut ctx: DjogiContext) {
    let gold = Author::create(&mut ctx, mk_author("gold"))
        .await
        .expect("create gold author");
    let bronze = Author::create(&mut ctx, mk_author("bronze"))
        .await
        .expect("create bronze author");

    Post::create(&mut ctx, mk_post(gold.id, "g1"))
        .await
        .expect("create gold post");
    Post::create(&mut ctx, mk_post(bronze.id, "b1"))
        .await
        .expect("create bronze post");

    let gold_authors = AuthorPublic::filter(|a| a.tier().eq("gold".to_string()))
        .selecting(AuthorPublic::id())
        .expect("no subquery modifiers");
    let rows = Post::objects()
        .filter(|f| f.author_id().in_visage(gold_authors))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch rows through IN visage subquery");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "g1");
}

#[djogi::djogi_test(sync_models = [Author, Post])]
async fn not_in_visage_excludes_matching_rows(mut ctx: DjogiContext) {
    let gold = Author::create(&mut ctx, mk_author("gold"))
        .await
        .expect("create gold author");
    let bronze = Author::create(&mut ctx, mk_author("bronze"))
        .await
        .expect("create bronze author");

    Post::create(&mut ctx, mk_post(gold.id, "g1"))
        .await
        .expect("create gold post");
    Post::create(&mut ctx, mk_post(bronze.id, "b1"))
        .await
        .expect("create bronze post");

    let gold_authors = AuthorPublic::filter(|a| a.tier().eq("gold".to_string()))
        .selecting(AuthorPublic::id())
        .expect("no subquery modifiers");
    let rows = Post::objects()
        .filter(|f| f.author_id().not_in_visage(gold_authors))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch rows through NOT IN visage subquery");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "b1");
}
