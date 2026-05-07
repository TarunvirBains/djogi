// Phase 7-Zero-2 T4 live + descriptor coverage for ambient PK fields.
//
// Two independent guarantees are exercised here:
//
// 1. **Descriptor emission.** Built-in PK-shaped types used outside the
//    framework-injected `id` slot lower to the same
//    [`FieldSqlType`](djogi::FieldSqlType) the PK-slot field would —
//    `HeerId` / `HeerIdRecencyBiased` → `BigInt`,
//    `RanjId` / `RanjIdRecencyBiased` → `Uuid`. These four assertions
//    run purely off `Model::descriptor()` and do not touch the database
//    (the `#[djogi::djogi_test]` harness still spins up a database so
//    the assertions live next to the live round-trip test below — this
//    keeps every Phase 7-Zero-2 T4 check in one file and matches the
//    `phase7_zero2_custom_pk_live.rs` layout).
//
// 2. **Live round-trip.** A model with one ambient `HeerId` column and
//    one ambient `RanjId` column round-trips through `tokio-postgres` /
//    `postgres-types` without any PK-slot codec arm: the macro's
//    generic user-field path is the same path used for any other
//    scalar, and `heeranjid`'s own `ToSql` / `FromSql` impls do the
//    wire work.

use djogi::prelude::*;
use djogi::types::{HeerId, HeerIdRecencyBiased, RanjId, RanjIdRecencyBiased};

// A model whose `id` is the default `HeerIdRecencyBiased` (post-T2) and
// whose four user columns each exercise one built-in PK family in the
// ambient-field position. The descriptor-only tests below walk this
// struct's emitted `ModelDescriptor::fields` to assert `sql_type`.
#[model(table = "phase7_zero2_t4_descriptor_ambient_pk")]
#[derive(Debug, Clone)]
pub struct DescriptorAmbient {
    pub heerid_col: HeerId,
    pub heer_desc_col: HeerIdRecencyBiased,
    pub ranjid_col: RanjId,
    pub ranj_desc_col: RanjIdRecencyBiased,
}

fn sql_type_of(field_name: &str) -> djogi::FieldSqlType {
    let fields = <DescriptorAmbient as Model>::descriptor().fields;
    fields
        .iter()
        .find(|f| f.name == field_name)
        .unwrap_or_else(|| panic!("field `{field_name}` not found in descriptor"))
        .sql_type
        .clone()
}

// Descriptor-only checks — no `#[djogi::djogi_test]` harness is needed
// because they run purely against the macro-emitted
// `ModelDescriptor::fields` table. Keeping them as plain `#[test]` fns
// lets them pass even in environments without `DATABASE_URL` (developer
// machines without Postgres, docs builds, etc.); the live round-trip
// below still exercises the actual codec path end-to-end when the
// integration DB is available.

#[test]
fn heerid_ambient_field_lowers_to_bigint() {
    assert!(matches!(
        sql_type_of("heerid_col"),
        djogi::FieldSqlType::BigInt
    ));
}

#[test]
fn heerid_recency_biased_ambient_field_lowers_to_bigint() {
    assert!(matches!(
        sql_type_of("heer_desc_col"),
        djogi::FieldSqlType::BigInt
    ));
}

#[test]
fn ranjid_ambient_field_lowers_to_uuid() {
    assert!(matches!(
        sql_type_of("ranjid_col"),
        djogi::FieldSqlType::Uuid
    ));
}

#[test]
fn ranjid_recency_biased_ambient_field_lowers_to_uuid() {
    assert!(matches!(
        sql_type_of("ranj_desc_col"),
        djogi::FieldSqlType::Uuid
    ));
}

// A model with one ambient `HeerId` column and one ambient `RanjId`
// column, used for the live round-trip. `id` keeps the post-T2 default
// (`HeerIdRecencyBiased`).
#[model(table = "phase7_zero2_t4_live_ambient_pk")]
#[derive(Debug, Clone)]
pub struct LiveAmbient {
    pub from_heerid: HeerId,
    pub to_ranjid: RanjId,
    pub label: String,
}

#[djogi::djogi_test(sync_models = [LiveAmbient])]
async fn ambient_heerid_and_ranjid_columns_round_trip(mut ctx: DjogiContext) {
    // Pre-generate the two ambient PK values server-side so the test
    // exercises the same `generate_*` paths the framework uses for the
    // `id` slot, just wired onto user columns instead.
    let ambient_heerid = <HeerId as PrimaryKeyDbGen>::generate(&mut ctx)
        .await
        .expect("generate() must succeed");
    let ambient_ranjid = <RanjId as PrimaryKeyDbGen>::generate(&mut ctx)
        .await
        .expect("generate() must succeed");

    let draft = LiveAmbient {
        from_heerid: ambient_heerid,
        to_ranjid: ambient_ranjid,
        label: "ambient-round-trip".to_string(),
        ..::std::default::Default::default()
    };

    let created = LiveAmbient::create(&mut ctx, draft)
        .await
        .expect("create must succeed");

    // The framework-injected `id` carries its own default; the two
    // ambient fields must round-trip the values we supplied above.
    assert_eq!(created.from_heerid, ambient_heerid);
    assert_eq!(created.to_ranjid, ambient_ranjid);
    assert_eq!(created.label, "ambient-round-trip");

    let fetched = LiveAmbient::get(&mut ctx, created.id)
        .await
        .expect("get must succeed");

    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.from_heerid, ambient_heerid);
    assert_eq!(fetched.to_ranjid, ambient_ranjid);
    assert_eq!(fetched.label, "ambient-round-trip");
}
