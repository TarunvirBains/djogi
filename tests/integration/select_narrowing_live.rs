// — visage queryset entry live test.
//
// Asserts that `{Visage}::filter(...)` is a working queryset entry point and
// The typed end-to-end assertion (`fetch_all` returns rows decoded into
// `UserPublic`) proves the visage queryset entry point round-trips through
// the public projection model. Exact emitted SQL inspection is a hidden
// framework hook and is intentionally not used in this ordinary integration
// source.

use djogi::prelude::*;

#[model(table = "users_narrow")]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public))]
    pub display_name: String,
    #[field(expose(self_view))]
    pub email: String,
    pub password_hash: String,
}

#[djogi::djogi_test(sync_models = [User])]
async fn visage_queryset_fetches_exposed_projection(mut ctx: DjogiContext) {
    // Seed one row through the source-model CRUD surface.
    User::create(
        &mut ctx,
        User {
            id: <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel(),
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            display_name: "Ada".to_string(),
            email: "ada@example.com".to_string(),
            password_hash: "secret".to_string(),
        },
    )
    .await
    .expect("create Ada");

    let rows: Vec<UserPublic> = UserPublic::filter(|f| f.display_name().eq("Ada".to_string()))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch_all projection");
    assert_eq!(rows.len(), 1, "exactly one row was seeded");
    assert_eq!(rows[0].display_name, "Ada");
}

#[djogi::djogi_test(sync_models = [User])]
async fn visage_queryset_filter_predicate_round_trips(mut ctx: DjogiContext) {
    User::create(
        &mut ctx,
        User {
            id: <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel(),
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            display_name: "Ada".to_string(),
            email: "ada@example.com".to_string(),
            password_hash: "secret".to_string(),
        },
    )
    .await
    .expect("create Ada");
    User::create(
        &mut ctx,
        User {
            id: <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel(),
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            display_name: "Grace".to_string(),
            email: "grace@example.com".to_string(),
            password_hash: "other".to_string(),
        },
    )
    .await
    .expect("create Grace");

    let rows: Vec<UserPublic> = UserPublic::filter(|f| f.display_name().eq("Ada".to_string()))
        .fetch_all(&mut ctx)
        .await
        .expect("fetch_all narrowed with predicate");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].display_name, "Ada");

    let count: i64 = UserPublic::filter(|f| f.display_name().eq("Grace".to_string()))
        .count(&mut ctx)
        .await
        .expect("count narrowed");
    assert_eq!(count, 1);
}
