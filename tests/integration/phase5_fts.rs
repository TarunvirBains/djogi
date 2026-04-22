//! Phase 5 Task 14 — Full-Text Search integration tests.
//!
//! Six tests that exercise the typed FTS query layer end-to-end against a
//! live Postgres database:
//!
//! 1. `fts_matches_basic_terms` — insert books, query, assert matches.
//! 2. `fts_rank_orders_by_relevance` — multi-result ranking via `ts_rank`.
//! 3. `fts_dictionary_honored` — English stemming collapses "running" → "run".
//! 4. `fts_multi_column_source` — `source = "title, body"` indexes both
//!    columns; matching against each independently works.
//! 5. `fts_generated_column_auto_populated` — the `search` GENERATED column is
//!    populated by Postgres automatically; no manual assignment needed.
//! 6. `fts_dictionary_change_is_alteration` — unit test (no DB): two
//!    `FtsDescriptor`s with different dictionaries compare not-equal, proving
//!    Phase 6's migration differ can detect the change.
//!
//! All DB-touching tests use `#[djogi::djogi_test]` — the Phase 5-Zero harness
//! that installs HeeRanjID, seeds node 1, and supplies a fresh `DjogiContext`
//! per test.

use djogi::prelude::*;
use postgres_types::ToSql;
use tokio_postgres::Row;

// ---------------------------------------------------------------------------
// Test model
// ---------------------------------------------------------------------------

/// A book with title + body text. The `search` tsvector column is GENERATED
/// ALWAYS AS by Postgres and is NOT declared as a struct field — the database
/// maintains it automatically. The FTS accessor `BookFields::search()` is
/// synthesized by the macro from the `fts(...)` attribute.
///
/// Declared with `fts(source = "title, body", dictionary = "english")` so
/// the macro emits:
/// - An `FtsDescriptor` into `ModelDescriptor::fts`.
/// - A `BookFields::search()` method returning an `FtsFieldRef<Book>`.
#[model(table = "book", fts(source = "title, body", dictionary = "english"))]
#[derive(Debug, Clone)]
pub struct Book {
    pub title: String,
    pub body: String,
}

// ---------------------------------------------------------------------------
// Setup helper
// ---------------------------------------------------------------------------

async fn setup_fts(ctx: &mut djogi::DjogiContext) {
    const BOOK_DDL: &str = include_str!("migrations/phase5/011_fts_book.sql");
    // Use `raw_ddl` (simple-query batch_execute) because the migration file
    // contains multiple statements (CREATE TABLE + CREATE INDEX). `raw_execute`
    // routes through prepare_cached which requires a single statement.
    ctx.raw_ddl(BOOK_DDL).await.expect("apply 011_fts_book.sql");
}

/// Insert a book row via raw SQL, explicitly omitting the `search` GENERATED
/// column so Postgres can compute it automatically.
async fn insert_book(ctx: &mut djogi::DjogiContext, title: &str, body: &str) {
    ctx.raw_execute(
        "INSERT INTO book (title, body) VALUES ($1, $2)",
        &[&title as &(dyn ToSql + Sync), &body as &(dyn ToSql + Sync)],
    )
    .await
    .expect("insert book");
}

// ---------------------------------------------------------------------------
// Test 1 — basic FTS match via the @@ operator
// ---------------------------------------------------------------------------

/// Inserts three books and queries for "planet". Expects exactly the two
/// books whose title or body contains the word "planet" to be returned.
#[djogi::djogi_test]
async fn fts_matches_basic_terms(mut ctx: djogi::DjogiContext) {
    setup_fts(&mut ctx).await;

    insert_book(&mut ctx, "Planet Earth", "A tour of our home planet.").await;
    insert_book(
        &mut ctx,
        "Mars Expedition",
        "The journey to the red planet.",
    )
    .await;
    insert_book(
        &mut ctx,
        "Ocean Depths",
        "Exploring the mysteries of the deep sea.",
    )
    .await;

    let rows: Vec<Row> = ctx
        .__query_all_for_macros(
            "SELECT title FROM book \
             WHERE search @@ to_tsquery('english', $1) \
             ORDER BY title",
            &[&"planet" as &(dyn ToSql + Sync)],
        )
        .await
        .expect("FTS query should succeed");

    assert_eq!(
        rows.len(),
        2,
        "expected 2 books matching 'planet', got {}",
        rows.len()
    );

    let titles: Vec<String> = rows
        .iter()
        .map(|r| r.try_get::<_, String>("title").expect("title column"))
        .collect();

    assert!(
        titles.contains(&"Mars Expedition".to_owned()),
        "expected Mars Expedition in results, got: {titles:?}"
    );
    assert!(
        titles.contains(&"Planet Earth".to_owned()),
        "expected Planet Earth in results, got: {titles:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 2 — ts_rank orders results by relevance
// ---------------------------------------------------------------------------

/// Inserts two books: one mentions "planet" many times (high relevance) and
/// one mentions it once (low relevance). Verifies ORDER BY ts_rank DESC puts
/// the denser match first.
#[djogi::djogi_test]
async fn fts_rank_orders_by_relevance(mut ctx: djogi::DjogiContext) {
    setup_fts(&mut ctx).await;

    // Book A: "planet" appears several times — higher rank expected.
    insert_book(
        &mut ctx,
        "Planet Guide",
        "This planet is a very interesting planet to visit. Our solar planet is special.",
    )
    .await;
    // Book B: "planet" appears once — lower rank expected.
    insert_book(&mut ctx, "Space Travel", "Visit our nearest planet once.").await;

    let rows: Vec<Row> = ctx
        .__query_all_for_macros(
            "SELECT title, ts_rank(search, to_tsquery('english', $1)) AS score \
             FROM book \
             WHERE search @@ to_tsquery('english', $1) \
             ORDER BY score DESC",
            &[&"planet" as &(dyn ToSql + Sync)],
        )
        .await
        .expect("rank query should succeed");

    assert!(
        rows.len() >= 2,
        "expected at least 2 results, got {}",
        rows.len()
    );

    let first_title: String = rows[0].try_get("title").expect("title on first row");
    let first_score: f32 = rows[0].try_get("score").expect("score on first row");
    let second_score: f32 = rows[1].try_get("score").expect("score on second row");

    assert_eq!(
        first_title, "Planet Guide",
        "Planet Guide (many 'planet' occurrences) should rank first; got: {first_title}"
    );
    assert!(
        first_score >= second_score,
        "first result score ({first_score}) must be >= second ({second_score})"
    );
}

// ---------------------------------------------------------------------------
// Test 3 — English dictionary honors stemming
// ---------------------------------------------------------------------------

/// Inserts a book mentioning "running". Queries for "run". The English
/// dictionary's stemmer maps "running" → "run", so the query must match.
#[djogi::djogi_test]
async fn fts_dictionary_honored(mut ctx: djogi::DjogiContext) {
    setup_fts(&mut ctx).await;

    // "running" — English dictionary stems this to "run".
    insert_book(
        &mut ctx,
        "Marathon Training",
        "Running every day improves your stamina and endurance.",
    )
    .await;
    insert_book(
        &mut ctx,
        "Cooking Basics",
        "How to boil water and steam rice properly.",
    )
    .await;

    // Query "run" — English stemmer should match "running" via stem "run".
    let rows: Vec<Row> = ctx
        .__query_all_for_macros(
            "SELECT title FROM book WHERE search @@ to_tsquery('english', $1)",
            &[&"run" as &(dyn ToSql + Sync)],
        )
        .await
        .expect("stemming query should succeed");

    assert_eq!(
        rows.len(),
        1,
        "expected exactly 1 book matching 'run' (stemmed from 'running'), got {}",
        rows.len()
    );

    let title: String = rows[0].try_get("title").expect("title column");
    assert_eq!(
        title, "Marathon Training",
        "wrong book matched stem 'run'; expected Marathon Training, got: {title}"
    );
}

// ---------------------------------------------------------------------------
// Test 4 — multi-column source indexes both title and body
// ---------------------------------------------------------------------------

/// Inserts two books: one where the search term appears only in the title,
/// one where it appears only in the body. Both must be found, confirming
/// that the `source = "title, body"` declaration indexes both columns.
#[djogi::djogi_test]
async fn fts_multi_column_source(mut ctx: djogi::DjogiContext) {
    setup_fts(&mut ctx).await;

    // "gravity" is only in the title.
    insert_book(
        &mut ctx,
        "Gravity Physics",
        "The study of fundamental forces.",
    )
    .await;
    // "photosynthesis" is only in the body.
    insert_book(
        &mut ctx,
        "Plant Life",
        "Photosynthesis powers all plant growth.",
    )
    .await;

    // Query "gravity" — should match by title.
    let gravity_rows: Vec<Row> = ctx
        .__query_all_for_macros(
            "SELECT title FROM book WHERE search @@ to_tsquery('english', $1)",
            &[&"gravity" as &(dyn ToSql + Sync)],
        )
        .await
        .expect("gravity query");
    assert_eq!(
        gravity_rows.len(),
        1,
        "expected 1 book matching 'gravity' (title-only term), got {}",
        gravity_rows.len()
    );
    let t: String = gravity_rows[0].try_get("title").unwrap();
    assert_eq!(t, "Gravity Physics");

    // Query "photosynthesis" — should match by body.
    let photo_rows: Vec<Row> = ctx
        .__query_all_for_macros(
            "SELECT title FROM book WHERE search @@ to_tsquery('english', $1)",
            &[&"photosynthesis" as &(dyn ToSql + Sync)],
        )
        .await
        .expect("photosynthesis query");
    assert_eq!(
        photo_rows.len(),
        1,
        "expected 1 book matching 'photosynthesis' (body-only term), got {}",
        photo_rows.len()
    );
    let t2: String = photo_rows[0].try_get("title").unwrap();
    assert_eq!(t2, "Plant Life");
}

// ---------------------------------------------------------------------------
// Test 5 — generated column is auto-populated (no manual insert needed)
// ---------------------------------------------------------------------------

/// Inserts a book by writing only `title` and `body`. Reads back the
/// `search` column and asserts Postgres populated it automatically. This
/// confirms the GENERATED ALWAYS AS definition in the migration is correct
/// and that application code never needs to supply a value for `search`.
#[djogi::djogi_test]
async fn fts_generated_column_auto_populated(mut ctx: djogi::DjogiContext) {
    setup_fts(&mut ctx).await;

    // Only title + body — no search value supplied.
    insert_book(
        &mut ctx,
        "Quantum Mechanics",
        "Wave-particle duality explained.",
    )
    .await;

    // Read back the search column as text to inspect its content.
    let rows: Vec<Row> = ctx
        .__query_all_for_macros(
            "SELECT search::text AS search_text FROM book WHERE title = $1",
            &[&"Quantum Mechanics" as &(dyn ToSql + Sync)],
        )
        .await
        .expect("select search column");

    assert_eq!(rows.len(), 1, "expected exactly one book row");

    let search_text: String = rows[0].try_get("search_text").expect("search_text column");

    assert!(
        !search_text.is_empty(),
        "generated search column should be non-empty after INSERT; got empty string"
    );

    // The tsvector must contain lexemes from title/body. Postgres stems
    // "quantum" → "quantum" and "mechanics" → "mechan". Either is fine.
    assert!(
        search_text.contains("quantum") || search_text.contains("mechan"),
        "tsvector should contain lexemes from title 'Quantum Mechanics'; got: {search_text}"
    );
}

// ---------------------------------------------------------------------------
// Test 6 — descriptor diff detects dictionary change as alteration (unit test)
// ---------------------------------------------------------------------------

/// Verifies the shape that Phase 6's migration differ will consume.
///
/// This is a pure unit test (no database required). It constructs two
/// `FtsDescriptor` values that differ only in `dictionary` and asserts they
/// compare not-equal via `PartialEq`. The differ contract: any change to
/// `FtsDescriptor.dictionary`, `source`, or `column` is a column-type
/// alteration that requires dropping and re-creating the generated column.
#[test]
fn fts_dictionary_change_is_alteration() {
    use djogi::FtsDescriptor;

    let d_english = FtsDescriptor {
        column: "search",
        source: "title, body",
        dictionary: "english",
    };
    let d_spanish = FtsDescriptor {
        column: "search",
        source: "title, body",
        dictionary: "spanish",
    };

    assert_ne!(
        d_english, d_spanish,
        "FtsDescriptors with different dictionaries must NOT be equal — \
         a dictionary change is a column-type alteration that Phase 6's \
         migration differ must treat as a drop + recreate"
    );

    // Identical descriptors MUST compare equal — the differ skips them.
    let d_english2 = FtsDescriptor {
        column: "search",
        source: "title, body",
        dictionary: "english",
    };
    assert_eq!(
        d_english, d_english2,
        "identical FtsDescriptors must be equal — differ should not emit DDL"
    );

    // Source list change is also an alteration (different tsvector expression).
    let d_title_only = FtsDescriptor {
        column: "search",
        source: "title",
        dictionary: "english",
    };
    assert_ne!(
        d_english, d_title_only,
        "changing the source list is an alteration; differ must detect it"
    );
}
