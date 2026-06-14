use djogi::prelude::*;

#[model(table = "vsq_corr_authors")]
#[derive(Debug, Clone)]
pub struct Author {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "vsq_corr_posts")]
#[derive(Debug, Clone)]
pub struct Post {
    #[field(expose(public))]
    pub author_id: HeerIdRecencyBiased,
    #[field(expose(public))]
    pub published: bool,
}

fn mk_author(name: &str) -> Author {
    Author {
        id: <HeerIdRecencyBiased as PrimaryKey>::sentinel(),
        created_at: time::OffsetDateTime::now_utc(),
        updated_at: time::OffsetDateTime::now_utc(),
        name: name.to_string(),
    }
}

fn mk_post(author_id: HeerIdRecencyBiased, published: bool) -> Post {
    Post {
        id: <HeerIdRecencyBiased as PrimaryKey>::sentinel(),
        created_at: time::OffsetDateTime::now_utc(),
        updated_at: time::OffsetDateTime::now_utc(),
        author_id,
        published,
    }
}

#[djogi::djogi_test(sync_models = [Author, Post])]
async fn correlated_exists_returns_authors_with_published_posts(mut ctx: DjogiContext) {
    let a1 = Author::create(&mut ctx, mk_author("a1"))
        .await
        .expect("create a1");
    let a2 = Author::create(&mut ctx, mk_author("a2"))
        .await
        .expect("create a2");

    Post::create(&mut ctx, mk_post(a1.id, true))
        .await
        .expect("create published post");
    Post::create(&mut ctx, mk_post(a2.id, false))
        .await
        .expect("create unpublished post");

    let has_published = VisageExists::new(PostPublic::filter(|p| {
        Q::Expression(p.published().as_expr().eq(Expr::literal(true)))
            & Q::Expression(
                p.author_id()
                    .as_expr()
                    .eq(AuthorOuterRef::id().as_qualified_expr()),
            )
    }))
    .expect("no subquery modifiers");
    let rows = Author::objects()
        .filter(|_| has_published.clone())
        .fetch_all(&mut ctx)
        .await
        .expect("fetch correlated EXISTS rows");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, a1.id);
    assert_eq!(rows[0].name, "a1");
}
