use djogi::prelude::*;

#[model(table = "book", fts(source = "title, body", dictionary = "english"))]
#[derive(Debug, Clone)]
pub struct Book {
    pub title: String,
    pub body: String,
}

async fn create_book(ctx: &mut djogi::DjogiContext, title: &str, body: &str) -> Book {
    Book::create(
        ctx,
        Book {
            title: title.to_string(),
            body: body.to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("create book")
}

#[djogi::djogi_test(sync_models = [Book])]
async fn fts_matches_basic_terms(mut ctx: djogi::DjogiContext) {
    create_book(&mut ctx, "Planet Earth", "A tour of our home planet.").await;
    create_book(
        &mut ctx,
        "Mars Expedition",
        "The journey to the red planet.",
    )
    .await;
    create_book(
        &mut ctx,
        "Ocean Depths",
        "Exploring the mysteries of the deep sea.",
    )
    .await;

    let titles: Vec<String> = Book::objects()
        .filter(|f| f.search().matches(TsQuery::new("planet")))
        .order_by(|f| f.title().asc())
        .fetch_all(&mut ctx)
        .await
        .expect("FTS query should succeed")
        .into_iter()
        .map(|book| book.title)
        .collect();

    assert_eq!(titles, vec!["Mars Expedition", "Planet Earth"]);
}

#[test]
fn fts_rank_expression_is_typed() {
    let _rank: Expr<f32> = BookFields::default().search().rank(TsQuery::new("planet"));
}

#[djogi::djogi_test(sync_models = [Book])]
async fn fts_dictionary_honors_stemming(mut ctx: djogi::DjogiContext) {
    create_book(
        &mut ctx,
        "Marathon Training",
        "Running every day improves your stamina and endurance.",
    )
    .await;
    create_book(
        &mut ctx,
        "Cooking Basics",
        "How to boil water and steam rice properly.",
    )
    .await;

    let matches = Book::objects()
        .filter(|f| f.search().matches(TsQuery::new("run")))
        .fetch_all(&mut ctx)
        .await
        .expect("stemming query should succeed");

    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].title, "Marathon Training");
}

#[djogi::djogi_test(sync_models = [Book])]
async fn fts_multi_column_source_matches_title_and_body(mut ctx: djogi::DjogiContext) {
    let title_match = create_book(
        &mut ctx,
        "Gravity Physics",
        "The study of fundamental forces.",
    )
    .await;
    let body_match = create_book(
        &mut ctx,
        "Plant Life",
        "Photosynthesis powers all plant growth.",
    )
    .await;

    let gravity = Book::objects()
        .filter(|f| f.search().matches(TsQuery::new("gravity")))
        .fetch_all(&mut ctx)
        .await
        .expect("title-source query");
    assert_eq!(
        gravity.iter().map(|book| book.id).collect::<Vec<_>>(),
        vec![title_match.id]
    );

    let photosynthesis = Book::objects()
        .filter(|f| f.search().matches(TsQuery::new("photosynthesis")))
        .fetch_all(&mut ctx)
        .await
        .expect("body-source query");
    assert_eq!(
        photosynthesis
            .iter()
            .map(|book| book.id)
            .collect::<Vec<_>>(),
        vec![body_match.id],
    );
}

#[test]
fn fts_dictionary_change_is_alteration() {
    use djogi::FtsDescriptor;

    let english = FtsDescriptor {
        column: "search",
        source: "title, body",
        dictionary: "english",
    };
    let spanish = FtsDescriptor {
        column: "search",
        source: "title, body",
        dictionary: "spanish",
    };
    let title_only = FtsDescriptor {
        column: "search",
        source: "title",
        dictionary: "english",
    };

    assert_ne!(english, spanish);
    assert_ne!(english, title_only);
    assert_eq!(english, english);
}
