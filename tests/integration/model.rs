// Integration tests. Uses throwaway test models — not framework types.

use djogi::prelude::*;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Test models — HeerId (default PK)
// ---------------------------------------------------------------------------

//  flipped the default `pk` to `HeerIdRecencyBiased`
// (internal `HeerIdDesc`). This test exercises the ascending-HeerId path
// explicitly — it asserts `PkType::HeerId`, `id sql_type == BigInt`, and
// `row.id.as_i64() > 0`. Pin the declaration so the flip doesn't silently
// change what the test exercises.
#[model(table = "posts", pk = HeerId)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Post {
    pub title: String,
    pub body: String,
    pub published: bool,
    pub view_count: i32,
}

// ---------------------------------------------------------------------------
// FromPgRow test ()
// ---------------------------------------------------------------------------
//
//  replaced the macro-emitted `sqlx::FromRow` impl with
// `FromPgRow` (ordinal decode + debug-build column-name guard). This
// test round-trips a row through `Post::create` so it exercises the
// full path (INSERT + `RETURNING <COLUMN_LIST>` + positional decode)
// that replaces the old `sqlx::query_as::<_, Post>` shape.

#[djogi::djogi_test(sync_models = [Post, Tag, Event, Product])]
async fn from_pg_row_deserializes_correctly(mut ctx: djogi::DjogiContext) {
    let row = Post::create(
        &mut ctx,
        Post {
            title: "Hello World".into(),
            body: "First post body".into(),
            published: false,
            view_count: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed via FromPgRow decode");

    assert_eq!(row.title, "Hello World");
    assert_eq!(row.body, "First post body");
    assert!(!row.published);
    assert_eq!(row.view_count, 0);
    assert!(row.id.as_i64() > 0, "DB-generated HeerId must be positive");
}

// ---------------------------------------------------------------------------
// CRUD tests
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [Post, Tag, Event, Product])]
async fn create_returns_full_row_with_generated_id(mut ctx: djogi::DjogiContext) {
    let post = Post::create(
        &mut ctx,
        Post {
            title: "My First Post".into(),
            body: "Hello, Djogi!".into(),
            published: false,
            view_count: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    assert!(
        post.id.as_i64() > 0,
        "id must be DB-generated positive HeerId"
    );
    assert_eq!(post.title, "My First Post");
    assert_eq!(post.body, "Hello, Djogi!");
    assert!(!post.published);
}

#[djogi::djogi_test(sync_models = [Post, Tag, Event, Product])]
async fn get_returns_correct_row(mut ctx: djogi::DjogiContext) {
    let created = Post::create(
        &mut ctx,
        Post {
            title: "Fetchable Post".into(),
            body: "Body text".into(),
            published: true,
            view_count: 42,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    let fetched = Post::get(&mut ctx, created.id)
        .await
        .expect("get should succeed");

    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.title, "Fetchable Post");
    assert_eq!(fetched.view_count, 42);
    assert!(fetched.published);
}

#[djogi::djogi_test(sync_models = [Post, Tag, Event, Product])]
async fn get_returns_not_found_for_missing_id(mut ctx: djogi::DjogiContext) {
    let missing_id =
        ::heeranjid::HeerId::from_i64(999_999_999).expect("999_999_999 is a valid HeerId");
    let result = Post::get(&mut ctx, missing_id).await;

    assert!(
        matches!(result, Err(DjogiError::NotFound { .. })),
        "expected NotFound, got {:?}",
        result
    );
}

#[djogi::djogi_test(sync_models = [Post, Tag, Event, Product])]
async fn save_updates_fields(mut ctx: djogi::DjogiContext) {
    let mut post = Post::create(
        &mut ctx,
        Post {
            title: "Original Title".into(),
            body: "Original body".into(),
            published: false,
            view_count: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    post.title = "Updated Title".into();
    post.published = true;
    post.save(&mut ctx).await.expect("save should succeed");

    let reloaded = Post::get(&mut ctx, post.id)
        .await
        .expect("get should succeed");
    assert_eq!(reloaded.title, "Updated Title");
    assert!(reloaded.published);
    assert_eq!(reloaded.body, "Original body");
}

#[djogi::djogi_test(sync_models = [Post, Tag, Event, Product])]
async fn delete_removes_row(mut ctx: djogi::DjogiContext) {
    let post = Post::create(
        &mut ctx,
        Post {
            title: "To Be Deleted".into(),
            body: "Gone soon".into(),
            published: false,
            view_count: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    let id = post.id;
    post.delete(&mut ctx).await.expect("delete should succeed");

    let result = Post::get(&mut ctx, id).await;
    assert!(
        matches!(result, Err(DjogiError::NotFound { .. })),
        "expected NotFound after delete, got {:?}",
        result
    );
}

#[djogi::djogi_test(sync_models = [Post, Tag, Event, Product])]
async fn refresh_from_db_returns_current_state(mut ctx: djogi::DjogiContext) {
    let post = Post::create(
        &mut ctx,
        Post {
            title: "Before Refresh".into(),
            body: "Stale body".into(),
            published: false,
            view_count: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    Post::objects()
        .filter(|f| f.id().eq(post.id))
        .update(|f| f.title().set("After Refresh".to_string()))
        .execute(&mut ctx)
        .await
        .expect("queryset update should succeed");

    // Our in-memory `post` is stale — refresh_from_db should return the new state.
    let refreshed = post
        .refresh_from_db(&mut ctx)
        .await
        .expect("refresh_from_db should succeed");

    assert_eq!(refreshed.title, "After Refresh");
    assert_eq!(refreshed.body, "Stale body");
}

// ---------------------------------------------------------------------------
// Serial PK model
// ---------------------------------------------------------------------------

#[model(table = "tags", pk = Serial)]
#[derive(Debug, Clone, PartialEq)]
pub struct Tag {
    pub name: String,
    pub color: String,
}

#[djogi::djogi_test(sync_models = [Post, Tag, Event, Product])]
async fn serial_pk_create_and_get(mut ctx: djogi::DjogiContext) {
    let tag = Tag::create(
        &mut ctx,
        Tag {
            name: "rust".into(),
            color: "#f74c00".into(),
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    assert!(tag.id > 0, "serial id must be positive");
    assert_eq!(tag.name, "rust");

    let fetched = Tag::get(&mut ctx, tag.id)
        .await
        .expect("get should find by i32 id");
    assert_eq!(fetched.name, "rust");
}

// ---------------------------------------------------------------------------
// RanjId PK model
// ---------------------------------------------------------------------------

#[model(table = "events", pk = RanjId)]
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub kind: String,
    pub payload: String,
}

#[djogi::djogi_test(sync_models = [Post, Tag, Event, Product])]
async fn ranjid_pk_create_and_get(mut ctx: djogi::DjogiContext) {
    let event = Event::create(
        &mut ctx,
        Event {
            kind: "user.signup".into(),
            payload: "{}".into(),
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    // RanjId wraps Uuid — check the underlying UUID isn't nil.
    assert!(!event.id.as_uuid().is_nil(), "RanjId must not be nil UUID");

    let fetched = Event::get(&mut ctx, event.id)
        .await
        .expect("get should find by RanjId");
    assert_eq!(fetched.kind, "user.signup");
}

// ---------------------------------------------------------------------------
// create_with_id + transaction tests
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [Post, Tag, Event, Product])]
async fn create_with_id_is_idempotent(mut ctx: djogi::DjogiContext) {
    // Simulate form pre-generation: allocate ID before user submits.
    let pre_generated_id = ::djogi::HeerId::generate(&mut ctx)
        .await
        .expect("typed HeerId generation should succeed");

    let post = Post::create_with_id(
        &mut ctx,
        pre_generated_id,
        Post {
            title: "Pre-generated".into(),
            body: "Idempotent insert".into(),
            published: false,
            view_count: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create_with_id should succeed");

    assert_eq!(
        post.id, pre_generated_id,
        "returned id must match pre-generated id"
    );

    // Second call with same id: ON CONFLICT DO NOTHING — should not error.
    // Limitation: when the conflict fires, RETURNING * returns no rows,
    // and the method falls back to returning the caller-supplied value with the
    // given id (not the original row's data). Full idempotent fetch is deferred
    // — see the DONE_WITH_CONCERNS note in crud.rs.
    let second = Post::create_with_id(
        &mut ctx,
        pre_generated_id,
        Post {
            title: "Different title".into(),
            body: "Different body".into(),
            published: true,
            view_count: 99,
            ..Default::default()
        },
    )
    .await
    .expect("idempotent re-insert should not error");

    assert_eq!(
        second.id, pre_generated_id,
        "id must match pre-generated id on conflict"
    );
}

#[djogi::djogi_test(sync_models = [Post, Tag, Event, Product])]
async fn crud_respects_transaction_boundary(mut ctx: djogi::DjogiContext) {
    // Proves BOTH sides of the transaction boundary:
    //   (a) commit path  — Post::create'd row IS visible after commit
    //   (b) rollback path — Post::create'd row is NOT visible after rollback
    //
    // Earlier revision only tested (b) which is a false positive: an
    // uncommitted transaction's row wouldn't be visible to the pool's other
    // connections REGARDLESS of whether the txn rolled back or just dropped.
    // We need both branches to actually prove the boundary works.

    // (a) commit — insert + save inside txn, commit, row must be visible
    let mut tx_commit_ctx = ctx.begin().await.unwrap();
    let committed = Post::create(
        &mut tx_commit_ctx,
        Post {
            title: "Committed".into(),
            body: "persists".into(),
            published: true,
            view_count: 1,
            ..Default::default()
        },
    )
    .await
    .expect("create inside commit txn should succeed");
    tx_commit_ctx.commit().await.unwrap();

    let fetched = Post::get(&mut ctx, committed.id)
        .await
        .expect("committed row must be visible");
    assert_eq!(fetched.title, "Committed");

    // (b) rollback — insert + save inside txn, rollback, row must NOT be visible
    let mut tx_rollback_ctx = ctx.begin().await.unwrap();
    let mut rolled_back = Post::create(
        &mut tx_rollback_ctx,
        Post {
            title: "Rolled Back".into(),
            body: "does not persist".into(),
            published: false,
            view_count: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create inside rollback txn should succeed");
    rolled_back
        .save(&mut tx_rollback_ctx)
        .await
        .expect("save inside rollback txn should succeed");
    tx_rollback_ctx.rollback().await.unwrap();

    let result = Post::get(&mut ctx, rolled_back.id).await;
    assert!(
        matches!(result, Err(DjogiError::NotFound { .. })),
        "rolled-back row must NOT be visible, got: {:?}",
        result
    );
}

// ---------------------------------------------------------------------------
// ModelDescriptor registration test
// ---------------------------------------------------------------------------

#[test]
fn model_descriptor_registered() {
    // inventory collects all ModelDescriptor submissions at link time.
    // `inventory::iter::<T>` is a zero-sized type implementing IntoIterator —
    // use it WITHOUT parentheses (not `inventory::iter::<T>()`).
    let descriptors: Vec<&::djogi::ModelDescriptor> = ::inventory::iter::<::djogi::ModelDescriptor>
        .into_iter()
        .collect();

    let post_desc = descriptors
        .iter()
        .find(|d| d.table_name == "posts")
        .expect("Post ModelDescriptor should be registered via inventory");

    assert_eq!(post_desc.type_name, "Post");
    assert_eq!(post_desc.table_name, "posts");
    assert!(matches!(post_desc.pk_type, ::djogi::PkType::HeerId));

    // fields includes framework columns (id, created_at, updated_at)
    // plus the 4 user fields = 7 total for HeerId-PK models
    // (3 framework + 4 user = 7).
    assert_eq!(
        post_desc.fields.len(),
        7,
        "descriptor must contain id + created_at + updated_at + 4 user fields, got {}",
        post_desc.fields.len()
    );

    // Framework fields appear first, in injection order.
    assert_eq!(post_desc.fields[0].name, "id");
    assert!(
        matches!(post_desc.fields[0].sql_type, ::djogi::FieldSqlType::BigInt),
        "id sql_type must be BigInt for HeerId model"
    );
    assert!(!post_desc.fields[0].nullable);

    assert_eq!(post_desc.fields[1].name, "created_at");
    assert!(
        matches!(
            post_desc.fields[1].sql_type,
            ::djogi::FieldSqlType::Timestamptz
        ),
        "created_at sql_type must be Timestamptz"
    );
    assert!(!post_desc.fields[1].nullable);

    assert_eq!(post_desc.fields[2].name, "updated_at");
    assert!(
        matches!(
            post_desc.fields[2].sql_type,
            ::djogi::FieldSqlType::Timestamptz
        ),
        "updated_at sql_type must be Timestamptz"
    );
    assert!(!post_desc.fields[2].nullable);

    // Per-field sql_type + nullable spot-checks covering each mapping branch.
    // Uses find() so these still work regardless of the framework-fields prefix.
    let field = |name: &str| {
        post_desc
            .fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("field `{name}` missing from descriptor"))
    };
    let title = field("title");
    assert!(matches!(title.sql_type, ::djogi::FieldSqlType::Text));
    assert!(!title.nullable);
    let view_count = field("view_count");
    assert!(matches!(
        view_count.sql_type,
        ::djogi::FieldSqlType::Integer
    ));
    assert!(!view_count.nullable);
    let published = field("published");
    assert!(matches!(published.sql_type, ::djogi::FieldSqlType::Boolean));
    assert!(!published.nullable);

    // Per-field defaults on every field (including framework fields,
    // which also have rationale=None, outbox_exclude=false, index_type=None).
    for f in post_desc.fields {
        assert!(
            f.rationale.is_none(),
            "field `{}` should have no rationale",
            f.name
        );
        assert!(
            !f.outbox_exclude,
            "field `{}` outbox_exclude should be false",
            f.name
        );
        assert!(
            f.index_type.is_none(),
            "field `{}` index_type should be None",
            f.name
        );
    }

    // Model-level defaults: all amended fields zero/None/empty.
    assert!(post_desc.partition_by.is_none());
    assert!(!post_desc.has_outbox);
    assert!(post_desc.idempotency_key.is_none());
    assert!(post_desc.tenant_key.is_none());
    assert!(post_desc.cache_ttl.is_none());
    assert!(post_desc.rationale.is_none());
    assert!(post_desc.indexes.is_empty());
}

#[model(table = "bounded_texts", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct BoundedText {
    #[field(max_length = 64)]
    pub title: String,
    pub body: String,
}

#[test]
fn varchar_max_length_descriptor_and_ddl_shape() {
    let descriptor = <BoundedText as ::djogi::prelude::Model>::descriptor();
    let title = descriptor
        .fields
        .iter()
        .find(|f| f.name == "title")
        .expect("`title` field must be present in descriptor");

    assert_eq!(title.sql_type, ::djogi::FieldSqlType::Varchar(64));
    assert_eq!(title.max_length, Some(64));

    let title_shape = descriptor
        .migration_shape()
        .columns
        .into_iter()
        .find(|column| column.name == "title")
        .expect("`title` must have migration shape");
    assert_eq!(title_shape.sql_type_text, "VARCHAR(64)");
}

// ==========================================================================
// TASK 10 — rich field types (Decimal, Vec<T>, time::Date, Option<String>)
// ==========================================================================

use rust_decimal::Decimal;

// `no_default` suppresses the generated `Default` impl because `time::Date`
// does not implement `Default`. The test uses explicit field initialisation.
#[model(table = "products", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct Product {
    pub name: String,
    pub price: Decimal,
    pub in_stock: bool,
    pub tags: Vec<String>,
    pub ratings: Vec<i32>,
    pub launch_date: ::time::Date,
    pub description: Option<String>,
}

#[model(table = "returning_pair_long_aliases", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct ReturningPairLongAliasSingle {
    // 50 chars: below Postgres boundary when prefixed.
    pub xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx: i32,
    // 52 chars: boundary +1 when prefixed with "__djogi_old." / "__djogi_new.".
    pub xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxa: i32,
    // 52 chars, same first 51 chars as previous field to model truncation
    // collision pressure in legacy alias projection styles.
    pub xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxb: i32,
}

#[djogi::djogi_test(sync_models = [Post, Tag, Event, Product])]
async fn rich_field_types_roundtrip(mut ctx: djogi::DjogiContext) {
    use rust_decimal_macros::dec;

    // Construct without ..Default::default() because time::Date does not
    // implement Default; all injected framework fields use sentinel values.
    let product = Product::create(
        &mut ctx,
        Product {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            name: "Djogi Framework".into(),
            price: dec!(49.99),
            in_stock: true,
            tags: vec!["rust".into(), "framework".into()],
            ratings: vec![5, 4, 5],
            launch_date: ::time::Date::from_calendar_date(2026, ::time::Month::April, 15).unwrap(),
            description: Some("A Model-first web framework".into()),
        },
    )
    .await
    .expect("create with rich fields should succeed");

    assert_eq!(product.name, "Djogi Framework");
    assert_eq!(product.price, dec!(49.99));
    assert!(product.in_stock, "bool field must round-trip on create");
    assert_eq!(
        product.tags,
        vec!["rust".to_string(), "framework".to_string()]
    );
    assert_eq!(product.ratings, vec![5, 4, 5]);
    assert_eq!(
        product.description,
        Some("A Model-first web framework".into())
    );

    // Round-trip through FromRow — fetch the row and assert the decoded values match.
    let fetched = Product::get(&mut ctx, product.id)
        .await
        .expect("get should find product");
    assert_eq!(fetched.price, dec!(49.99));
    assert!(fetched.in_stock, "bool field must round-trip on FromRow");
    assert_eq!(
        fetched.launch_date,
        ::time::Date::from_calendar_date(2026, ::time::Month::April, 15).unwrap()
    );
    assert_eq!(fetched.tags.len(), 2);
    assert_eq!(fetched.ratings, vec![5, 4, 5]);
}

// ---------------------------------------------------------------------------
// Djogi#180 — PG18 OLD/NEW RETURNING integration tests
// ---------------------------------------------------------------------------
//
// The `Post` model (table = "posts", pk = HeerId) is reused below. All tests
// are additive and do not change the model schema.

#[djogi::djogi_test(sync_models = [Post, Tag, Event, Product])]
async fn update_returning_pair_returns_old_and_new_snapshots(mut ctx: djogi::DjogiContext) {
    let post = Post::create(
        &mut ctx,
        Post {
            title: "Before".into(),
            body: "initial body".into(),
            published: false,
            view_count: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    let original_id = post.id;
    let original_created_at = post.created_at;

    // Mutate the in-memory instance and call update_returning_pair.
    let mut to_update = post;
    to_update.title = "After".into();
    to_update.published = true;
    to_update.view_count = 42;

    let pair = to_update
        .update_returning_pair(&mut ctx)
        .await
        .expect("update_returning_pair should succeed");

    // old and new share the same primary key and created_at.
    assert_eq!(pair.old.id, original_id, "old.id must equal new.id");
    assert_eq!(pair.new.id, original_id, "new.id must equal original id");
    assert_eq!(
        pair.old.created_at, original_created_at,
        "created_at must not change on update"
    );
    assert_eq!(pair.new.created_at, original_created_at);

    // old side preserves the pre-update values.
    assert_eq!(pair.old.title, "Before");
    assert!(!pair.old.published);
    assert_eq!(pair.old.view_count, 0);

    // new side reflects the applied changes.
    assert_eq!(pair.new.title, "After");
    assert!(pair.new.published);
    assert_eq!(pair.new.view_count, 42);

    // updated_at must not regress.
    assert!(
        pair.new.updated_at >= pair.old.updated_at,
        "new.updated_at must not be before old.updated_at"
    );
}

#[djogi::djogi_test(sync_models = [Post, Tag, Event, Product])]
async fn update_returning_pair_reflects_db_in_new_row(mut ctx: djogi::DjogiContext) {
    // Verify that pair.new can be fetched from the DB and matches.
    let post = Post::create(
        &mut ctx,
        Post {
            title: "DB Check".into(),
            body: "check body".into(),
            published: false,
            view_count: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    let id = post.id;
    let mut to_update = post;
    to_update.title = "DB Check Updated".into();
    let pair = to_update
        .update_returning_pair(&mut ctx)
        .await
        .expect("update_returning_pair should succeed");

    // Fetch fresh from DB and compare with pair.new.
    let from_db = Post::get(&mut ctx, id)
        .await
        .expect("get should find updated post");
    assert_eq!(from_db.title, pair.new.title);
    assert_eq!(from_db.updated_at, pair.new.updated_at);
}

#[djogi::djogi_test(sync_models = [Post, Tag, Event, Product])]
async fn delete_returning_returns_pre_delete_snapshot(mut ctx: djogi::DjogiContext) {
    let post = Post::create(
        &mut ctx,
        Post {
            title: "To Delete".into(),
            body: "delete body".into(),
            published: true,
            view_count: 7,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    let id = post.id;
    let title = post.title.clone();
    let view_count = post.view_count;

    let deleted = post
        .delete_returning(&mut ctx)
        .await
        .expect("delete_returning should succeed");

    // The returned snapshot matches what was in the DB.
    assert_eq!(deleted.id, id);
    assert_eq!(deleted.title, title);
    assert_eq!(deleted.view_count, view_count);
    assert!(deleted.published);

    // Row is gone from the DB.
    let result = Post::get(&mut ctx, id).await;
    assert!(
        matches!(result, Err(DjogiError::NotFound { .. })),
        "row should be gone after delete_returning, got {result:?}"
    );
}

#[djogi::djogi_test(sync_models = [Post, Tag, Event, Product])]
async fn delete_returning_not_found_for_missing_row(mut ctx: djogi::DjogiContext) {
    let post = Post::create(
        &mut ctx,
        Post {
            title: "To Delete".into(),
            body: "delete body".into(),
            published: true,
            view_count: 7,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    let deleted = post
        .delete_returning(&mut ctx)
        .await
        .expect("first delete_returning should succeed");

    let result = deleted.delete_returning(&mut ctx).await;
    assert!(
        matches!(result, Err(DjogiError::NotFound { .. })),
        "delete_returning on missing row should return typed NotFound, got {result:?}"
    );
}

#[djogi::djogi_test(sync_models = [Post, Tag, Event, Product])]
async fn update_returning_pair_not_found_for_missing_row(mut ctx: djogi::DjogiContext) {
    let post = Post::create(
        &mut ctx,
        Post {
            title: "Versionless".into(),
            body: "before delete".into(),
            published: false,
            view_count: 3,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    let deleted = post
        .delete_returning(&mut ctx)
        .await
        .expect("first delete_returning should succeed");

    let mut stale = deleted;
    stale.view_count += 1;
    stale.title = "after-delete-update".into();

    let result = stale.update_returning_pair(&mut ctx).await;
    assert!(
        matches!(result, Err(DjogiError::NotFound { .. })),
        "update_returning_pair on missing row should return typed NotFound, got {result:?}"
    );
}

#[djogi::djogi_test(sync_models = [ReturningPairLongAliasSingle])]
async fn update_returning_pair_handles_boundary_and_collision_oriented_aliases(
    mut ctx: djogi::DjogiContext,
) {
    assert_eq!(
        "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx".len(),
        50,
        "first long column name should be 50 chars"
    );
    assert_eq!(
        "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxa".len(),
        52,
        "second long column name should be 52 chars"
    );
    assert_eq!(
        "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxb".len(),
        52,
        "third long column name should be 52 chars"
    );

    let row = ReturningPairLongAliasSingle::create(
        &mut ctx,
        ReturningPairLongAliasSingle {
            xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx: 10,
            xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxa: 20,
            xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxb: 30,
            ..Default::default()
        },
    )
    .await
    .expect("create should decode via canonical FromPgRow ordering");

    let mut updated = row;
    updated.xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx = 1010;
    updated.xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxa = 2020;
    updated.xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxb = 3030;

    let pair = updated
        .update_returning_pair(&mut ctx)
        .await
        .expect("update_returning_pair should decode stable alias projection");

    assert_eq!(pair.old.id, pair.new.id);
    assert_eq!(pair.old.created_at, pair.new.created_at);
    assert_eq!(
        pair.old.xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx, 10,
        "short/near-boundary long name should preserve old value"
    );
    assert_eq!(
        pair.old
            .xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxa,
        20,
        "second long name should preserve pre-update value"
    );
    assert_eq!(
        pair.old
            .xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxb,
        30,
        "third long name should preserve pre-update value"
    );
    assert_eq!(
        pair.new.xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx, 1010,
        "first long name should reflect new value"
    );
    assert_eq!(
        pair.new
            .xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxa,
        2020,
        "second long name should reflect new value"
    );
    assert_eq!(
        pair.new
            .xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxb,
        3030,
        "third long name should reflect new value"
    );
}
