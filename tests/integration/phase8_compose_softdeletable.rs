//! Phase 8α T2.3 integration tests: `#[derive(SoftDeletable)]` proc
//! macro + manual `QuerySet::not_deleted()` helper.
//!
//! What this file pins:
//!
//! 1. `#[derive(SoftDeletable)]` emits an `impl ::djogi::SoftDeletable
//!    for #ident` block whose `deleted_at()` getter returns
//!    `Option<DateTime>` copied from the adopter-declared
//!    `deleted_at: Option<DateTime>` field (Path B per Phase 8 v3
//!    line 866).
//! 2. `QuerySet::<M>::not_deleted()` (where `M: SoftDeletable`)
//!    composes a `deleted_at IS NULL` leaf onto the condition tree.
//!    Calling it on a queryset that returns mixed live/trashed rows
//!    narrows the result to the live rows only.
//! 3. **Counter-test:** the default `objects()` chain (without
//!    `.not_deleted()`) returns trashed rows alongside live ones.
//!    This pins the spec-locked deferral at line 971 — automatic
//!    default-filter composition is deferred to Phase 8γ T6 once the
//!    `Q<T>` substrate lands. When 8γ T6 ships, this test breaks
//!    loudly so the implementer can flip the assertion (to one row)
//!    instead of silently changing the framework's default behaviour.
//!
//! # One model per test — coherence
//!
//! `impl SoftDeletable for T` is a coherent impl: only one per `T`
//! per crate. Tests that need separate fixture data declare separate
//! model types over distinct tables. Tests that share fixture data
//! reuse the same model + table.
//!
//! # Fixture strategy
//!
//! Each test provisions its own table inline via
//! `ctx.raw_execute(...)`. `#[djogi::djogi_test]` already installs
//! HeeRanjID schema, seeds node 1, and sets `heer.node_id = '1'`
//! before the test body runs.

use djogi::SoftDeletable;
use djogi::prelude::*;
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// Test 1 — `deleted_at()` returns `Some(now)` when the field is set.
// ---------------------------------------------------------------------------

#[derive(SoftDeletable)]
#[model(table = "soft_getter")]
#[derive(Debug, Clone)]
pub struct SoftGetter {
    pub note: String,
    pub deleted_at: Option<djogi::DateTime>,
}

async fn setup_soft_getter(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE soft_getter (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            note        TEXT        NOT NULL,
            deleted_at  TIMESTAMPTZ
        )",
        &[],
    )
    .await
    .expect("create soft_getter table");
}

#[djogi::djogi_test]
async fn softdeletable_getter_round_trip(mut ctx: djogi::DjogiContext) {
    setup_soft_getter(&mut ctx).await;

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

#[derive(SoftDeletable)]
#[model(table = "soft_filter")]
#[derive(Debug, Clone)]
pub struct SoftFilter {
    pub note: String,
    pub deleted_at: Option<djogi::DateTime>,
}

async fn setup_soft_filter(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE soft_filter (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            note        TEXT        NOT NULL,
            deleted_at  TIMESTAMPTZ
        )",
        &[],
    )
    .await
    .expect("create soft_filter table");
}

#[djogi::djogi_test]
async fn softdeletable_not_deleted_filter_excludes_deleted(mut ctx: djogi::DjogiContext) {
    setup_soft_filter(&mut ctx).await;

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

    // Manual exclusion via the T2.3 helper. In Phase 8γ T6 this
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
// Test 3 — Counter-test: the default `objects()` chain (no
// `.not_deleted()`) STILL returns trashed rows.
//
// **This test pins the spec-locked deferral at line 971
// (RESOLVED 2026-05-03, lens, locked).** When Phase 8γ T6 lands
// automatic default-filter composition under the new `Q<T>`
// substrate, this assertion will start failing — at which point the
// implementer must flip the count from 2 to 1 and add a comment that
// 8γ T6 made auto-composition active. Failing loudly is the whole
// point: the tripwire ensures default-query semantics never change
// silently.
// ---------------------------------------------------------------------------

#[derive(SoftDeletable)]
#[model(table = "soft_default")]
#[derive(Debug, Clone)]
pub struct SoftDefault {
    pub note: String,
    pub deleted_at: Option<djogi::DateTime>,
}

async fn setup_soft_default(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE soft_default (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            note        TEXT        NOT NULL,
            deleted_at  TIMESTAMPTZ
        )",
        &[],
    )
    .await
    .expect("create soft_default table");
}

#[djogi::djogi_test]
async fn softdeletable_default_query_includes_deleted_pre_8gamma(mut ctx: djogi::DjogiContext) {
    setup_soft_default(&mut ctx).await;

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

    // No `.not_deleted()` on the chain — the default `objects()`
    // call must still return both rows in Phase 8α. When 8γ T6
    // lands automatic default-filter composition, this expectation
    // changes to 1 row (the live one). The failure on this
    // assertion is the tripwire: it forces an explicit acknowledgment
    // that default-query semantics are about to change cluster-wide.
    let rows = SoftDefault::objects()
        .fetch_all(&mut ctx)
        .await
        .expect("fetch_all should succeed");

    assert_eq!(
        rows.len(),
        2,
        "Phase 8α T2.3 ships only the manual `.not_deleted()` helper — \
         automatic default-filter composition is deferred to Phase 8γ T6 \
         (spec line 971, RESOLVED 2026-05-03, lens, locked). \
         When 8γ T6 lands, this assertion must flip to 1 row, and a \
         comment recording the change should be added below.",
    );
}
