//! Cluster 8δ T7.2 — runtime checks for the auto-emitted
//! `impl Cacheable for {Model}` and `impl DeltaSyncCacheable for {Model}`.
//!
//! Ships in `djogi-macros/tests/` rather than `djogi/tests/` because
//! the surface under test is what the macro emits — `#[derive(Model)]`
//! is `djogi-macros`-owned, the trait re-exports are `djogi`-owned,
//! and putting the integration test alongside the macro keeps the
//! provenance clear. The lihaaf compile-pass fixtures
//! (`tests/compile_pass/phase8_t7_cacheable_*.rs`) cover the
//! macro-emission side from a standalone-fixture angle; this file is
//! the in-crate side — uses `#[derive(Model)]` directly through the
//! `djogi` dev-dep prelude and asserts on the resulting trait
//! contract.
//!
//! The PK-strategy fan-out (one model per built-in `pk = X` value)
//! pins `Cacheable::Id` to the type `inject::expand` actually injects.
//! A future change that flips the lowering for one of these
//! identifiers without updating the macro emit fails on the
//! `assert_id_type::<Model, ExpectedId>()` call at monomorphisation.
//!
//! Spec anchor:
//!   docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md
//!   §3 commit T7.2 — "Test names + assertions" bullet, plus the
//!   T7.2 phase amendment block (Codex Finding 6 PK-variant fan-out).

use djogi::prelude::*;

// Bring the re-exported `Cacheable` / `DeltaSyncCacheable` traits into
// scope for method dispatch. `djogi::types::Cacheable` is the macro-
// routing path the auto-emit targets; `djogi::cache::Cacheable`
// (re-exported from the same sassi trait via `djogi/src/cache.rs`)
// resolves to the same trait, so importing either is equivalent.
// We import via `djogi::types` to mirror the macro-emission target
// path exactly.
use djogi::__private::pg::SqlAccumulator;
use djogi::__private::query::{PortablePredicateError, SqlEmitContext};
use djogi::types::{BasicPredicate, Cacheable, DeltaSyncCacheable, IntoBasicPredicate};

// ---------------------------------------------------------------------------
// PK-variant fixtures — one model per built-in `pk = X` strategy plus the
// `primary_key!`-declared custom variant. Each fixture exists solely so the
// per-PK `assert_id_type` test can pin `Cacheable::Id` to the expected
// concrete type.
// ---------------------------------------------------------------------------

/// Default `#[model]` declaration. Per Phase 7-Zero-2 T2 the implicit
/// PK strategy is `HeerIdDesc` (recency-biased), so the auto-emitted
/// `Cacheable::Id` resolves to `HeerIdDesc`.
#[model(table = "phase8_t7_cacheable_emit_default")]
#[derive(Debug, Clone)]
pub struct DefaultModel {
    pub label: String,
}

/// Explicit `pk = HeerId` (ascending HeerId — the pre-Phase-7-Zero-2
/// historical default).
#[model(table = "phase8_t7_cacheable_emit_heerid", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct HeerIdModel {
    pub label: String,
}

/// Explicit `pk = RanjId` (UUIDv8, ascending).
#[model(table = "phase8_t7_cacheable_emit_ranjid", pk = RanjId)]
#[derive(Debug, Clone)]
pub struct RanjIdModel {
    pub label: String,
}

/// Explicit `pk = HeerIdDesc` (the canonical recency-biased ID).
#[model(table = "phase8_t7_cacheable_emit_heerid_desc", pk = HeerIdDesc)]
#[derive(Debug, Clone)]
pub struct HeerIdDescModel {
    pub label: String,
}

/// Explicit `pk = RanjIdDesc` (recency-biased UUIDv8).
#[model(table = "phase8_t7_cacheable_emit_ranjid_desc", pk = RanjIdDesc)]
#[derive(Debug, Clone)]
pub struct RanjIdDescModel {
    pub label: String,
}

/// `pk = Serial` lookup table. The injected `id` is `i32`, so
/// `Cacheable::Id` must resolve to `i32`. Custom-PK Cacheable bounds
/// (`Hash + Eq + Clone + Ord + Send + Sync + 'static`) are satisfied
/// by `i32` upstream.
#[model(table = "phase8_t7_cacheable_emit_serial", pk = Serial)]
#[derive(Debug, Clone)]
pub struct SerialModel {
    pub label: String,
}

// `primary_key!`-declared custom PK type. The newtype wraps `i64` and
// the auto-derive set added in Cluster 8δ T7.2 (Ord / PartialOrd /
// serde::Serialize / Deserialize on top of the previous Debug / Clone /
// Copy / PartialEq / Eq / Hash) ensures the inner value passes the
// `Cacheable::Id: Hash + Eq + Clone + Ord + Send + Sync + 'static`
// bound when the auto-emitted `impl Cacheable for CustomPkModel`
// binds `type Id = MyAppId`.
djogi::primary_key! {
    pub struct MyAppId(i64);
    sql_type = "BIGINT";
    default_sql = "0";
    bulk_sql = "SELECT 0::bigint AS id FROM generate_series(1, $1)";
}

/// Adopter-declared custom PK. `Cacheable::Id` resolves to `MyAppId`.
#[model(table = "phase8_t7_cacheable_emit_custom", pk = MyAppId)]
#[derive(Debug, Clone)]
pub struct CustomPkModel {
    pub label: String,
}

#[derive(DjogiEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[djogi_enum(name = "phase8_t7_cacheable_emit_status", rename_all = "snake_case")]
pub enum CacheableEmitStatus {
    Active,
    Retired,
}

/// User-defined Postgres enum field. The model macro sees only the declared
/// Rust type path, so portable SQL emission depends on the runtime codec
/// registered by `#[derive(DjogiEnum)]`.
#[model(table = "phase8_t7_cacheable_emit_enum", no_default)]
#[derive(Debug, Clone)]
pub struct EnumFieldModel {
    pub status: CacheableEmitStatus,
}

/// Nullable user-defined Postgres enum field. Covers the `Option<Enum>` and
/// `.some()` payload shapes separately from the non-null enum field above.
#[model(table = "phase8_t7_cacheable_emit_optional_enum", no_default)]
#[derive(Debug, Clone)]
pub struct OptionalEnumFieldModel {
    pub status: Option<CacheableEmitStatus>,
}

/// Tracked enum fields exercise the direct `Tracked<T>` declaration shape; the
/// predicate payload is `Tracked<Enum>` rather than the bare enum value.
#[model(table = "phase8_t7_cacheable_emit_tracked_enum", no_default)]
#[derive(Debug, Clone)]
pub struct TrackedEnumFieldModel {
    pub status: ::djogi::Tracked<CacheableEmitStatus>,
    pub optional_status: ::djogi::Tracked<Option<CacheableEmitStatus>>,
}

/// Protected enum fields must stay outside the portable runtime fallback:
/// protected codecs can change storage semantics in ways Punnu cannot mirror.
#[model(table = "phase8_t7_cacheable_emit_protected_enum", no_default)]
#[derive(Debug, Clone)]
pub struct ProtectedEnumFieldModel {
    #[field(protected(sensitivity = "none"))]
    pub status: CacheableEmitStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SameSqlCustomStatus(CacheableEmitStatus);

impl djogi::descriptor::DjogiSqlType for SameSqlCustomStatus {
    const SQL_TYPE: &'static str = "phase8_t7_cacheable_emit_status";
}

impl djogi::query::DjogiPortableEq for SameSqlCustomStatus {}

impl djogi::__private::postgres_types::ToSql for SameSqlCustomStatus {
    fn to_sql(
        &self,
        ty: &djogi::__private::postgres_types::Type,
        out: &mut djogi::__private::bytes::BytesMut,
    ) -> Result<djogi::__private::postgres_types::IsNull, Box<dyn std::error::Error + Sync + Send>>
    {
        <CacheableEmitStatus as djogi::__private::postgres_types::ToSql>::to_sql(&self.0, ty, out)
    }

    fn accepts(ty: &djogi::__private::postgres_types::Type) -> bool {
        <CacheableEmitStatus as djogi::__private::postgres_types::ToSql>::accepts(ty)
    }

    djogi::__private::postgres_types::to_sql_checked!();
}

impl<'a> djogi::__private::postgres_types::FromSql<'a> for SameSqlCustomStatus {
    fn from_sql(
        ty: &djogi::__private::postgres_types::Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        <CacheableEmitStatus as djogi::__private::postgres_types::FromSql>::from_sql(ty, raw)
            .map(Self)
    }

    fn accepts(ty: &djogi::__private::postgres_types::Type) -> bool {
        <CacheableEmitStatus as djogi::__private::postgres_types::FromSql>::accepts(ty)
    }
}

#[model(table = "phase8_t7_cacheable_emit_same_sql_custom", no_default)]
#[derive(Debug, Clone)]
pub struct SameSqlCustomFieldModel {
    pub status: SameSqlCustomStatus,
}

/// Watermark-override fixture — `expires_at` (a user field) replaces
/// the default `updated_at`. `time::OffsetDateTime` does not implement
/// `Default`, so the model carries `no_default` to skip the
/// `inject::generate_default_impl` pass.
#[model(
    table = "phase8_t7_cacheable_emit_watermark",
    watermark_field = "expires_at",
    no_default
)]
#[derive(Debug, Clone)]
pub struct WatermarkModel {
    pub label: String,
    pub expires_at: ::djogi::types::DateTime,
}

// ---------------------------------------------------------------------------
// Per-PK Cacheable::Id assertions. Each test threads the model and the
// expected `Cacheable::Id` type through a generic helper; if the macro
// emitted the wrong associated type the call site fails to monomorphise
// with a "type mismatch" error pointing at the `assert_id_type` line.
// ---------------------------------------------------------------------------

fn assert_id_type<T: Cacheable<Id = ExpectedId>, ExpectedId>()
where
    ExpectedId: ::std::hash::Hash + Eq + Clone + Ord + Send + Sync + 'static,
{
}

fn assert_portable_eq<T: ::djogi::DjogiPortableEq>() {}

fn emit_predicate_sql<M, P>(portable: P) -> Result<(String, u32), PortablePredicateError>
where
    M: Model,
    P: IntoBasicPredicate<M>,
{
    let BasicPredicate::Field(fp) = portable.into_basic_predicate() else {
        panic!("predicate should lower to a field predicate");
    };

    let mut acc = SqlAccumulator::new("");
    <M as Model>::__djogi_emit_field_predicate(&mut acc, &fp, SqlEmitContext::root())?;
    Ok((acc.sql().to_owned(), acc.bind_count()))
}

#[test]
fn relation_and_array_wrappers_are_portable_eq() {
    assert_portable_eq::<::djogi::ForeignKey<DefaultModel>>();
    assert_portable_eq::<::djogi::OneToOneField<DefaultModel>>();
    assert_portable_eq::<Vec<i32>>();
    assert_portable_eq::<Option<Vec<i32>>>();
}

#[test]
fn cacheable_emitted_for_heerid_pk() {
    assert_id_type::<HeerIdModel, ::djogi::types::HeerId>();
}

#[test]
fn cacheable_emitted_for_ranjid_pk() {
    assert_id_type::<RanjIdModel, ::djogi::types::RanjId>();
}

#[test]
fn cacheable_emitted_for_heerid_desc_pk() {
    // The default `#[model]` declaration also lowers to `HeerIdDesc`
    // (per `attrs.rs:1064` — `pk.unwrap_or(PkStrategy::HeerIdDesc)`),
    // so both spellings resolve to the same `Cacheable::Id` type.
    assert_id_type::<HeerIdDescModel, ::djogi::types::HeerIdDesc>();
    assert_id_type::<DefaultModel, ::djogi::types::HeerIdDesc>();
}

#[test]
fn cacheable_emitted_for_ranjid_desc_pk() {
    assert_id_type::<RanjIdDescModel, ::djogi::types::RanjIdDesc>();
}

#[test]
fn cacheable_emitted_for_serial_pk() {
    // `pk = Serial` → injected `id: i32`. `Cacheable::Id` must follow.
    assert_id_type::<SerialModel, i32>();
}

#[test]
fn cacheable_emitted_for_custom_pk() {
    // `primary_key!`-declared types satisfy `Cacheable::Id`'s
    // `Hash + Eq + Clone + Ord + Send + Sync + 'static` bound only
    // because the macro now auto-derives `PartialOrd` + `Ord` (T7.2;
    // `primary_key_macro.rs:299-324`). If those derives are dropped,
    // this test fails at the `assert_id_type` call site with a clean
    // bound error, not at a downstream Cacheable use site.
    assert_id_type::<CustomPkModel, MyAppId>();
    assert_portable_eq::<MyAppId>();
}

#[test]
fn custom_pk_id_field_preserves_portable_membership_surface() {
    let _filtered = CustomPkModel::objects().filter(|f| f.id().in_(vec![MyAppId(0)]));
}

#[test]
fn custom_pk_id_field_emits_portable_membership_sql() {
    let portable = <CustomPkModel as Cacheable>::fields()
        .id()
        .in_(vec![MyAppId(0)]);
    let basic = portable.into_basic_predicate();
    let BasicPredicate::Field(fp) = basic else {
        panic!("custom PK membership should lower to a field predicate");
    };

    let mut acc = SqlAccumulator::new("");
    let result = <CustomPkModel as Model>::__djogi_emit_field_predicate(
        &mut acc,
        &fp,
        SqlEmitContext::root(),
    );
    assert!(
        result.is_ok(),
        "custom PK membership SQL emission should be supported: {result:?}"
    );
}

#[test]
fn djogi_enum_field_is_portable_eq() {
    assert_portable_eq::<CacheableEmitStatus>();
    let _filtered = EnumFieldModel::objects().filter(|f| {
        f.status().eq(CacheableEmitStatus::Active)
            & f.status().in_(vec![
                CacheableEmitStatus::Active,
                CacheableEmitStatus::Retired,
            ])
    });
}

#[test]
fn djogi_enum_field_emits_portable_sql() {
    let (sql, bind_count) = emit_predicate_sql::<EnumFieldModel, _>(
        <EnumFieldModel as Cacheable>::fields()
            .status()
            .eq(CacheableEmitStatus::Active),
    )
    .expect("DjogiEnum equality SQL emission should be supported");
    assert_eq!(sql, "status = $1");
    assert_eq!(bind_count, 1);

    let (sql, bind_count) = emit_predicate_sql::<EnumFieldModel, _>(
        <EnumFieldModel as Cacheable>::fields().status().in_(vec![
            CacheableEmitStatus::Active,
            CacheableEmitStatus::Retired,
        ]),
    )
    .expect("DjogiEnum membership SQL emission should be supported");
    assert_eq!(sql, "status IN ($1, $2)");
    assert_eq!(bind_count, 2);
}

#[test]
fn optional_djogi_enum_field_emits_portable_sql() {
    let (sql, bind_count) = emit_predicate_sql::<OptionalEnumFieldModel, _>(
        <OptionalEnumFieldModel as Cacheable>::fields()
            .status()
            .eq(Some(CacheableEmitStatus::Active)),
    )
    .expect("Option<DjogiEnum> equality SQL emission should be supported");
    assert_eq!(sql, "status = $1");
    assert_eq!(bind_count, 1);

    let (sql, bind_count) = emit_predicate_sql::<OptionalEnumFieldModel, _>(
        <OptionalEnumFieldModel as Cacheable>::fields()
            .status()
            .in_(vec![Some(CacheableEmitStatus::Active), None]),
    )
    .expect("Option<DjogiEnum> membership SQL emission should be supported");
    assert_eq!(sql, "(status IS NULL OR status IN ($1))");
    assert_eq!(bind_count, 1);

    let (sql, bind_count) = emit_predicate_sql::<OptionalEnumFieldModel, _>(
        <OptionalEnumFieldModel as Cacheable>::fields()
            .status()
            .is_null(),
    )
    .expect("Option<DjogiEnum> null test SQL emission should be supported");
    assert_eq!(sql, "status IS NULL");
    assert_eq!(bind_count, 0);

    let (sql, bind_count) = emit_predicate_sql::<OptionalEnumFieldModel, _>(
        <OptionalEnumFieldModel as Cacheable>::fields()
            .status()
            .is_not_null(),
    )
    .expect("Option<DjogiEnum> non-null test SQL emission should be supported");
    assert_eq!(sql, "status IS NOT NULL");
    assert_eq!(bind_count, 0);

    let (sql, bind_count) = emit_predicate_sql::<OptionalEnumFieldModel, _>(
        <OptionalEnumFieldModel as Cacheable>::fields()
            .status()
            .some()
            .not_in(vec![CacheableEmitStatus::Retired]),
    )
    .expect("PresentField<DjogiEnum> membership SQL emission should be supported");
    assert_eq!(sql, "(status IS NOT NULL AND status NOT IN ($1))");
    assert_eq!(bind_count, 1);
}

#[test]
fn tracked_djogi_enum_field_emits_portable_sql() {
    let (sql, bind_count) = emit_predicate_sql::<TrackedEnumFieldModel, _>(
        <TrackedEnumFieldModel as Cacheable>::fields()
            .status()
            .eq(CacheableEmitStatus::Active),
    )
    .expect("Tracked<DjogiEnum> equality SQL emission should be supported");
    assert_eq!(sql, "status = $1");
    assert_eq!(bind_count, 1);

    let (sql, bind_count) = emit_predicate_sql::<TrackedEnumFieldModel, _>(
        <TrackedEnumFieldModel as Cacheable>::fields()
            .status()
            .in_(vec![
                CacheableEmitStatus::Active,
                CacheableEmitStatus::Retired,
            ]),
    )
    .expect("Tracked<DjogiEnum> membership SQL emission should be supported");
    assert_eq!(sql, "status IN ($1, $2)");
    assert_eq!(bind_count, 2);

    let (sql, bind_count) = emit_predicate_sql::<TrackedEnumFieldModel, _>(
        <TrackedEnumFieldModel as Cacheable>::fields()
            .optional_status()
            .eq(Some(CacheableEmitStatus::Retired)),
    )
    .expect("Tracked<Option<DjogiEnum>> equality SQL emission should be supported");
    assert_eq!(sql, "optional_status = $1");
    assert_eq!(bind_count, 1);

    let (sql, bind_count) = emit_predicate_sql::<TrackedEnumFieldModel, _>(
        <TrackedEnumFieldModel as Cacheable>::fields()
            .optional_status()
            .in_(vec![Some(CacheableEmitStatus::Active), None]),
    )
    .expect("Tracked<Option<DjogiEnum>> membership SQL emission should be supported");
    assert_eq!(sql, "(optional_status IS NULL OR optional_status IN ($1))");
    assert_eq!(bind_count, 1);
}

#[test]
fn djogi_enum_field_fallback_rejects_protected_and_same_sql_non_enum_fields() {
    let protected = emit_predicate_sql::<ProtectedEnumFieldModel, _>(
        <ProtectedEnumFieldModel as Cacheable>::fields()
            .status()
            .eq(CacheableEmitStatus::Active),
    );
    match protected {
        Err(PortablePredicateError::UnsupportedFieldType { field }) => {
            assert_eq!(field, "status");
        }
        other => panic!("protected DjogiEnum field should stay unsupported, got {other:?}"),
    }

    let same_sql_custom = emit_predicate_sql::<SameSqlCustomFieldModel, _>(
        <SameSqlCustomFieldModel as Cacheable>::fields()
            .status()
            .eq(SameSqlCustomStatus(CacheableEmitStatus::Active)),
    );
    match same_sql_custom {
        Err(PortablePredicateError::UnsupportedFieldType { field }) => {
            assert_eq!(field, "status");
        }
        other => panic!("same-SQL non-DjogiEnum field should stay unsupported, got {other:?}"),
    }
}

/// `pk = None` skips Cacheable emission entirely. Asserting absence
/// requires a separate lihaaf compile_fail fixture
/// (`tests/compile_fail/phase8_t7_cacheable_skipped_for_pk_none.rs`)
/// because absence-of-impl is not directly probable at runtime.
/// This stub names the asserted invariant for grep-discoverability.
#[test]
fn cacheable_skipped_for_pk_none() {
    // Intentionally empty — see the compile_fail fixture noted above.
    // Keeping the test name in this file pins the grep-discoverable
    // contract alongside the positive-emission tests.
}

// ---------------------------------------------------------------------------
// `Cacheable::Fields` associated-type pin (issue #121). The auto-emitted
// `impl Cacheable` MUST set `type Fields = {Model}Fields`, where
// `{Model}Fields` is the ZST companion emitted by `model::stubs::expand`
// (and re-used by every `QuerySet::filter(|f| ...)` closure call site).
//
// Without this pin a regression that:
//   * accidentally let sassi-codegen run `generate_fields_struct` would
//     surface an E0428 (`{Model}Fields defined twice`) at expand time —
//     caught upstream of this assertion;
//   * silently wired `type Fields = ()` (or any other unintended type)
//     through some future plumbing change would otherwise compile cleanly
//     — only this assertion catches that scenario.
// ---------------------------------------------------------------------------

fn assert_fields_type<T, Expected>()
where
    T: Cacheable<Fields = Expected>,
    Expected: ::std::default::Default + ::std::marker::Send + ::std::marker::Sync + 'static,
{
}

/// `Cacheable::Fields` for every PK strategy resolves to the djogi-emitted
/// `{Model}Fields` companion — never `()`, never a sassi-codegen-emitted
/// collision struct. This is the load-bearing surface check for the
/// Cluster 2 issue #121 cutover (route Cacheable emit through
/// `sassi_codegen::generate_cacheable_impl` with
/// `CacheableFieldsMode::external(...)`).
#[test]
fn cacheable_fields_is_djogi_companion() {
    assert_fields_type::<DefaultModel, DefaultModelFields>();
    assert_fields_type::<HeerIdModel, HeerIdModelFields>();
    assert_fields_type::<RanjIdModel, RanjIdModelFields>();
    assert_fields_type::<HeerIdDescModel, HeerIdDescModelFields>();
    assert_fields_type::<RanjIdDescModel, RanjIdDescModelFields>();
    assert_fields_type::<SerialModel, SerialModelFields>();
    assert_fields_type::<CustomPkModel, CustomPkModelFields>();
    assert_fields_type::<WatermarkModel, WatermarkModelFields>();
}

/// `Cacheable::fields()` must construct the ZST through the same
/// `{Model}Fields::new()` constructor `model::stubs::expand` emits — the
/// resulting handle is `Default + Send + Sync + 'static` and round-trips
/// through trait dispatch. Calling the trait method (rather than the
/// inherent `::new()`) proves the macro wired the constructor expression
/// correctly through `CacheableFieldsMode::external`'s `constructor` slot.
#[test]
fn cacheable_fields_constructor_returns_zst() {
    let _f: DefaultModelFields = <DefaultModel as Cacheable>::fields();
    let _f: HeerIdModelFields = <HeerIdModel as Cacheable>::fields();
    let _f: SerialModelFields = <SerialModel as Cacheable>::fields();
    let _f: CustomPkModelFields = <CustomPkModel as Cacheable>::fields();
    let _f: WatermarkModelFields = <WatermarkModel as Cacheable>::fields();
}

// ---------------------------------------------------------------------------
// Behaviour assertions — `Cacheable::id(&self)` clones the `id` field;
// `DeltaSyncCacheable::watermark(&self)` clones the watermark field.
// These are general-shape tests that don't change per PK strategy
// (every emitted impl goes through the same hand-roll body).
// ---------------------------------------------------------------------------

/// `Cacheable::id(&self)` clones the `id` field. The runtime check
/// confirms the emitted body is `self.id.clone()` — a model with
/// `id` zero-valued (the default-impl sentinel) round-trips through
/// `Cacheable::id` to exactly the same value.
#[test]
fn cacheable_id_returns_self_id_field() {
    let m = DefaultModel::default();
    let cached_id = <DefaultModel as Cacheable>::id(&m);
    // `HeerIdDesc::sentinel()` is the default-impl zero value per
    // `inject::generate_default_impl`; the auto-emitted `id()` must
    // return the same value through the trait dispatch.
    assert_eq!(cached_id, m.id);
}

/// `DeltaSyncCacheable::Watermark` resolves to the type of the field
/// named by `#[model(watermark_field = "expires_at")]` — `DateTime`
/// in this fixture. Without the override, the watermark defaults to
/// `updated_at: DateTime` (always present per Phase 7 framework-field
/// injection).
#[test]
fn delta_sync_cacheable_watermark_uses_named_field() {
    fn assert_watermark_type<T: DeltaSyncCacheable<Watermark = ::djogi::types::DateTime>>() {}
    assert_watermark_type::<WatermarkModel>();

    // The default-watermark branch — `DefaultModel` has no
    // `watermark_field` override, so the macro falls back to
    // `updated_at`, which is also `DateTime`. Same `Watermark` type
    // resolves through both paths.
    assert_watermark_type::<DefaultModel>();
}

/// `DeltaSyncCacheable::watermark(&self)` clones the named field.
/// For `WatermarkModel`, that's `expires_at`; for `DefaultModel`,
/// it's `updated_at`. Both flow through the same trait dispatch.
///
/// `WatermarkModel` carries `#[model(no_default)]` because
/// `time::OffsetDateTime` does not implement `Default` — every
/// field must be initialised explicitly. We use `OffsetDateTime::UNIX_EPOCH`
/// for both timestamp slots since the test only exercises trait
/// dispatch, not real-DB roundtrip.
#[test]
fn delta_sync_cacheable_watermark_returns_field_value() {
    let epoch = ::djogi::types::DateTime::UNIX_EPOCH;
    let m = WatermarkModel {
        id: <::djogi::types::HeerIdDesc as ::djogi::primary_key::PrimaryKey>::sentinel(),
        created_at: epoch,
        updated_at: epoch,
        label: "test".to_string(),
        expires_at: epoch,
    };
    let watermark = <WatermarkModel as DeltaSyncCacheable>::watermark(&m);
    assert_eq!(watermark, m.expires_at);

    let d = DefaultModel::default();
    let default_watermark = <DefaultModel as DeltaSyncCacheable>::watermark(&d);
    assert_eq!(default_watermark, d.updated_at);
}
