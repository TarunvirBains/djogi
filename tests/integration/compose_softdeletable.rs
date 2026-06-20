// .6 integration tests: `#[model(soft_deletable)]` opt-in
// (supersedes .3's `#[derive(SoftDeletable)]`) + manual
// `QuerySet::not_deleted()` helper.
//
// What this file pins:
//
// 1. `#[model(soft_deletable)]` emits an `impl ::djogi::SoftDeletable
//    for #ident` block whose `deleted_at()` getter returns
//    `Option<DateTime>` copied from the adopter-declared
//    `deleted_at: Option<DateTime>` field (Path B per v3
//    line 866).
// 2. `QuerySet::<M>::not_deleted()` (where `M: SoftDeletable`)
//    composes a `deleted_at IS NULL` leaf onto the condition tree.
//    Calling it on a queryset that returns mixed live/trashed rows
//    narrows the result to the live rows only.
// 3. **Counter-test:** the default `objects()` chain (without
//    `.not_deleted()`) returns trashed rows alongside live ones.
//    This pins the spec-locked deferral at line 971 — automatic
//    default-filter composition is deferred to  once the
//    `Q<T>` substrate lands. When 8γ  ships, this test breaks
//    loudly so the implementer can flip the assertion (to one row)
//    instead of silently changing the framework's default behaviour.
//
// # One model per test — coherence
//
// `impl SoftDeletable for T` is a coherent impl: only one per `T`
// per crate. Tests that need separate fixture data declare separate
// model types over distinct tables. Tests that share fixture data
// reuse the same model + table.
//
// # Fixture strategy
//
// Each test provisions its model through `sync_models`, then exercises
// the typed model, queryset, and trait APIs only.

use djogi::SoftDeletable;
use djogi::prelude::*;
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// Test 1 — `deleted_at()` returns `Some(now)` when the field is set.
// ---------------------------------------------------------------------------

#[model(table = "soft_getter", soft_deletable)]
#[derive(Debug, Clone)]
pub struct SoftGetter {
    pub note: String,
    pub deleted_at: Option<djogi::DateTime>,
}

#[djogi::djogi_test(sync_models = [SoftGetter])]
async fn softdeletable_getter_round_trip(mut ctx: djogi::DjogiContext) {
    let when = OffsetDateTime::now_utc();
    let row = SoftGetter::create(
        &mut ctx,
        SoftGetter {
            note: "first".into(),
            deleted_at: Some(when),
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    // Refresh from DB so we compare against the value Postgres
    // round-tripped (it gets normalised to microsecond precision).
    let fresh = row
        .refresh_from_db(&mut ctx)
        .await
        .expect("refresh should succeed");

    let getter_value = SoftDeletable::deleted_at(&fresh);
    assert!(
        getter_value.is_some(),
        "SoftDeletable::deleted_at() must return Some(_) for a row whose deleted_at column is non-NULL",
    );

    // The DB-stored timestamp must match what we put in (modulo
    // Postgres' microsecond truncation — compare to the second).
    let returned = getter_value.expect("Some(_) checked above");
    assert_eq!(
        returned.unix_timestamp(),
        when.unix_timestamp(),
        "SoftDeletable::deleted_at() must round-trip the value stored in the DB",
    );

    // Confirm the bound is usable at the framework boundary: code
    // that wants to talk generically about "models with soft-delete
    // semantics" can accept any `M: djogi::SoftDeletable`.
    fn is_trashed<M: djogi::SoftDeletable>(m: &M) -> bool {
        m.deleted_at().is_some()
    }
    assert!(is_trashed(&fresh));
}

// ---------------------------------------------------------------------------
// Test 2 — `.not_deleted()` excludes rows whose `deleted_at` is
// non-NULL.
// ---------------------------------------------------------------------------

#[model(table = "soft_filter", soft_deletable)]
#[derive(Debug, Clone)]
pub struct SoftFilter {
    pub note: String,
    pub deleted_at: Option<djogi::DateTime>,
}

#[djogi::djogi_test(sync_models = [SoftFilter])]
async fn softdeletable_not_deleted_filter_excludes_deleted(mut ctx: djogi::DjogiContext) {
    let _live = SoftFilter::create(
        &mut ctx,
        SoftFilter {
            note: "live".into(),
            deleted_at: None,
            ..Default::default()
        },
    )
    .await
    .expect("create live row should succeed");

    let _trashed = SoftFilter::create(
        &mut ctx,
        SoftFilter {
            note: "trashed".into(),
            deleted_at: Some(OffsetDateTime::now_utc()),
            ..Default::default()
        },
    )
    .await
    .expect("create trashed row should succeed");

    // Manual exclusion via the .3 helper. In  this
    // call site will become redundant once auto-composition lands;
    // until then the helper is the only path that excludes
    // soft-deleted rows.
    let rows = SoftFilter::objects()
        .not_deleted()
        .fetch_all(&mut ctx)
        .await
        .expect("fetch_all should succeed");

    assert_eq!(
        rows.len(),
        1,
        "QuerySet::not_deleted() must filter out rows whose deleted_at is non-NULL",
    );
    assert_eq!(
        rows[0].note, "live",
        "the live row is the one that survives the .not_deleted() filter",
    );
    assert!(
        SoftDeletable::deleted_at(&rows[0]).is_none(),
        "the surviving row's deleted_at column must be NULL",
    );
}

// ---------------------------------------------------------------------------
// Test 3 — the default `objects()` chain excludes soft-deleted rows.
//
// `#[model(soft_deletable)]` makes `objects()` apply a `deleted_at IS NULL`
// default filter automatically. An adopter who calls `objects()` with no
// further qualification sees only live rows; deleted rows require the
// explicit `objects_including_deleted()` bypass (see Test 4).
// ---------------------------------------------------------------------------

#[model(table = "soft_default", soft_deletable)]
#[derive(Debug, Clone)]
pub struct SoftDefault {
    pub note: String,
    pub deleted_at: Option<djogi::DateTime>,
}

#[djogi::djogi_test(sync_models = [SoftDefault])]
async fn softdeletable_objects_excludes_deleted_by_default(mut ctx: djogi::DjogiContext) {
    let _live = SoftDefault::create(
        &mut ctx,
        SoftDefault {
            note: "live".into(),
            deleted_at: None,
            ..Default::default()
        },
    )
    .await
    .expect("create live row should succeed");

    let _trashed = SoftDefault::create(
        &mut ctx,
        SoftDefault {
            note: "trashed".into(),
            deleted_at: Some(OffsetDateTime::now_utc()),
            ..Default::default()
        },
    )
    .await
    .expect("create trashed row should succeed");

    let rows = SoftDefault::objects()
        .fetch_all(&mut ctx)
        .await
        .expect("fetch_all should succeed");

    assert_eq!(
        rows.len(),
        1,
        "objects() on a soft-deletable model must exclude rows whose deleted_at is non-NULL",
    );
    assert_eq!(
        rows[0].note, "live",
        "the live row is the one that survives the default soft-delete filter",
    );
    assert!(
        djogi::SoftDeletable::deleted_at(&rows[0]).is_none(),
        "the surviving row's deleted_at column must be NULL",
    );
}

// ---------------------------------------------------------------------------
// Test 4 — objects_including_deleted() returns ALL rows, including deleted.
// ---------------------------------------------------------------------------

#[model(table = "soft_bypass", soft_deletable)]
#[derive(Debug, Clone)]
pub struct SoftBypass {
    pub note: String,
    pub deleted_at: Option<djogi::DateTime>,
}

#[djogi::djogi_test(sync_models = [SoftBypass])]
async fn softdeletable_objects_including_deleted_returns_all(mut ctx: djogi::DjogiContext) {
    let _live = SoftBypass::create(
        &mut ctx,
        SoftBypass {
            note: "live".into(),
            deleted_at: None,
            ..Default::default()
        },
    )
    .await
    .expect("create live row should succeed");

    let _trashed = SoftBypass::create(
        &mut ctx,
        SoftBypass {
            note: "trashed".into(),
            deleted_at: Some(OffsetDateTime::now_utc()),
            ..Default::default()
        },
    )
    .await
    .expect("create trashed row should succeed");

    let rows = SoftBypass::objects_including_deleted()
        .fetch_all(&mut ctx)
        .await
        .expect("fetch_all should succeed");

    assert_eq!(
        rows.len(),
        2,
        "objects_including_deleted() must bypass the soft-delete default filter and return deleted rows",
    );
}
