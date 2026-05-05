//! Phase 7-Zero-2 T10 — visage queryset entry + SELECT narrowing live test.
//!
//! Asserts that `{Visage}::filter(...)` is a working queryset entry point and
//! that the emitted SELECT projects only the visage's exposed columns. Non-
//! exposed columns (`email`, `password_hash` here) must never appear in the
//! emitted SQL.
//!
//! # Why this asserts on the SQL string directly
//!
//! `pg_stat_statements` requires server-side `shared_preload_libraries`
//! configuration that is not available on every test environment, and the
//! "last call" semantics around statement normalisation are too fragile to
//! anchor a correctness test on. Instead, the visage queryset exposes its
//! emitted SQL via the `#[doc(hidden)]` `VisageQuerySet::__sql_for_test`
//! method — the test inspects the exact string the runtime would send to
//! Postgres, which is the most direct evidence we can collect.
//!
//! The end-to-end assertion (`fetch_all` returns the seeded row decoded into
//! `UserPublic`) is the second witness: the narrowed SELECT must round-trip
//! through `FromPgRow for UserPublic` and yield the right values.

use djogi::prelude::*;
// Cluster 8γ Stage 2 (T6.9b): `Condition` retired from the prelude;
// reachable via the unstable internal namespace for in-tree consumers
// that still pattern-match on it. Adopter code composing through the
// public `Q<T>` algebra never needs this import.
use djogi::query::internal::Condition;

#[model(table = "phase7_zero2_t10_users_narrow")]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(public))]
    pub display_name: String,
    #[field(expose(self_view))]
    pub email: String,
    pub password_hash: String,
}

async fn setup(ctx: &mut DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE phase7_zero2_t10_users_narrow (
            id            BIGINT      PRIMARY KEY DEFAULT heerid_next_desc(),
            created_at    TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at    TIMESTAMPTZ NOT NULL    DEFAULT now(),
            display_name  TEXT        NOT NULL,
            email         TEXT        NOT NULL,
            password_hash TEXT        NOT NULL
         )",
        &[],
    )
    .await
    .expect("CREATE TABLE phase7_zero2_t10_users_narrow");
}

#[djogi::djogi_test]
async fn visage_queryset_emits_narrowed_select(mut ctx: DjogiContext) {
    setup(&mut ctx).await;

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

    // Step 1: assert the SQL the visage queryset emits is column-narrowed.
    let qs = UserPublic::filter(|_| Condition::True);
    let sql = qs.__sql_for_test();
    assert!(
        sql.contains("display_name"),
        "narrowed SELECT must include exposed column `display_name` — got: {sql}",
    );
    assert!(
        !sql.contains("email"),
        "narrowed SELECT must NOT include `email` (exposed only on self_view) — got: {sql}",
    );
    assert!(
        !sql.contains("password_hash"),
        "narrowed SELECT must NOT include `password_hash` (not exposed on any scope) — got: {sql}",
    );

    // Step 2: end-to-end fetch_all round-trips the seeded row through the
    // narrowed SELECT and the visage's narrow `FromPgRow` decoder.
    let rows: Vec<UserPublic> = UserPublic::filter(|_| Condition::True)
        .fetch_all(&mut ctx)
        .await
        .expect("fetch_all narrowed");
    assert_eq!(rows.len(), 1, "exactly one row was seeded");
    assert_eq!(rows[0].display_name, "Ada");
}

#[djogi::djogi_test]
async fn visage_queryset_filter_predicate_round_trips(mut ctx: DjogiContext) {
    setup(&mut ctx).await;

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

    let count: i64 = UserPublic::filter(|_| Condition::True)
        .count(&mut ctx)
        .await
        .expect("count narrowed");
    assert_eq!(count, 2);
}
