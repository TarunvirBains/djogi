//! Phase 7-Zero-2 T3 live-DB coverage for `djogi::primary_key!`.
//!
//! Exercises the full round-trip on a custom PK type declared through the
//! helper macro: `#[model]` injects `id: MyLiveId`, the table uses the
//! emitted `default_sql` to populate `id` server-side, the `RETURNING`
//! decode rehydrates it via the emitted `FromSql`, and `get()` round-trips
//! by primary key.
//!
//! The PK is a `BIGINT` backed by a `CREATE SEQUENCE` so the test does not
//! depend on HeeRanjId's `generate_id()` — the point is to validate the
//! macro-emitted codec, not the HeeRanjId schema.

use djogi::prelude::*;

djogi::primary_key! {
    pub struct MyLiveId(i64);
    sql_type = "BIGINT";
    default_sql = "nextval('live_custom_pk_seq')";
}

#[model(table = "live_custom_pk_rows", pk = MyLiveId)]
#[derive(Debug, Clone)]
pub struct LiveRow {
    pub label: String,
}

async fn setup(ctx: &mut DjogiContext) {
    ctx.raw_execute("CREATE SEQUENCE live_custom_pk_seq START 1", &[])
        .await
        .expect("CREATE SEQUENCE must succeed");
    ctx.raw_execute(
        "CREATE TABLE live_custom_pk_rows (
            id          BIGINT      PRIMARY KEY DEFAULT nextval('live_custom_pk_seq'),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            label       TEXT        NOT NULL
         )",
        &[],
    )
    .await
    .expect("CREATE TABLE must succeed");
}

#[djogi::djogi_test]
async fn custom_pk_create_and_fetch_round_trip(mut ctx: DjogiContext) {
    setup(&mut ctx).await;

    let created = LiveRow::create(
        &mut ctx,
        LiveRow {
            label: "round-trip".into(),
            ..Default::default()
        },
    )
    .await
    .expect("create() must succeed with a DB-defaulted custom PK");

    // The custom PK must have been populated from the sequence — not the
    // sentinel (0) the `Default` impl started with. This also verifies the
    // macro-emitted `FromSql` decoded the BIGINT row back into the newtype.
    assert_ne!(
        created.id,
        MyLiveId(0),
        "DB-sourced custom PK must not be the sentinel"
    );
    assert_eq!(created.label, "round-trip");

    let fetched = LiveRow::get(&mut ctx, created.id)
        .await
        .expect("get() by custom PK must resolve the row we just inserted");
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.label, "round-trip");
}

#[djogi::djogi_test]
async fn custom_pk_descriptor_carries_kind_through_inventory(mut ctx: DjogiContext) {
    // Descriptor-side assertion — no DB writes, but run inside the
    // `#[djogi_test]` harness so the shared Phase-7-Zero-2 integration
    // target owns the bootstrap consistently.
    let _ = &mut ctx;

    let descriptor = <LiveRow as ::djogi::model::Model>::descriptor();
    match descriptor.pk_type {
        ::djogi::PkType::Custom(kind) => {
            assert_eq!(kind.type_name, "MyLiveId");
            assert_eq!(kind.sql_type, "BIGINT");
            assert_eq!(kind.default_sql, "nextval('live_custom_pk_seq')");
        }
        other => panic!("expected PkType::Custom(..), got {other:?}"),
    }
}
