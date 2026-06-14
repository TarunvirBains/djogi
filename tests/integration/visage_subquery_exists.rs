use djogi::prelude::*;

#[model(table = "vsq_exists_authors")]
#[derive(Debug, Clone)]
pub struct Author {
    #[field(expose(public))]
    pub tier: String,
}

#[model(table = "vsq_exists_posts")]
#[derive(Debug, Clone)]
pub struct Post {
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

fn mk_post(title: &str) -> Post {
    Post {
        id: <HeerIdRecencyBiased as PrimaryKey>::sentinel(),
        created_at: time::OffsetDateTime::now_utc(),
        updated_at: time::OffsetDateTime::now_utc(),
        title: title.to_string(),
    }
}

#[djogi::djogi_test(sync_models = [Author, Post])]
async fn exists_visage_filters_when_any_row_matches(mut ctx: DjogiContext) {
    Author::create(&mut ctx, mk_author("gold"))
        .await
        .expect("create gold author");
    let post = Post::create(&mut ctx, mk_post("p"))
        .await
        .expect("create post");

    let any_gold = VisageExists::new(AuthorPublic::filter(|a| a.tier().eq("gold".to_string())))
        .expect("no subquery modifiers");
    let rows = Post::objects()
        .filter(|_| any_gold.clone())
        .fetch_all(&mut ctx)
        .await
        .expect("fetch rows through EXISTS visage subquery");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, post.id);
    assert_eq!(rows[0].title, "p");
}

#[djogi::djogi_test(sync_models = [Author, Post])]
async fn not_exists_visage_filters_when_no_row_matches(mut ctx: DjogiContext) {
    Author::create(&mut ctx, mk_author("bronze"))
        .await
        .expect("create bronze author");
    let post = Post::create(&mut ctx, mk_post("p"))
        .await
        .expect("create post");

    let no_gold = VisageExists::new(AuthorPublic::filter(|a| a.tier().eq("gold".to_string())))
        .expect("no subquery modifiers")
        .not_exists();
    let rows = Post::objects()
        .filter(|_| no_gold.clone())
        .fetch_all(&mut ctx)
        .await
        .expect("fetch rows through NOT EXISTS visage subquery");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, post.id);
}
