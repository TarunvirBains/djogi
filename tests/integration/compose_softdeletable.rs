// Integration tests: `#[model(soft_deletable)]` opt-in, the automatic
// soft-delete default filter on `objects()`, the explicit
// `objects_including_deleted()` bypass, and the
// `QuerySet::not_deleted()` helper.
//
// What this file pins:
//
// 1. `#[model(soft_deletable)]` emits an `impl ::djogi::SoftDeletable
//    for #ident` block whose `deleted_at()` getter returns
//    `Option<DateTime>` copied from the adopter-declared
//    `deleted_at: Option<DateTime>` field.
// 2. `QuerySet::<M>::not_deleted()` (where `M: SoftDeletable`)
//    composes a `deleted_at IS NULL` leaf onto the condition tree.
// 3. `objects()` on a soft-deletable model excludes deleted rows by
//    default; `objects_including_deleted()` bypasses that filter.
// 4. The default filter composes through proxy models, tenant-keyed
//    models, visages, prefetch, select_related, and `in_bulk`.
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
use djogi::auth::AuthContext;
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

    // Explicit exclusion via the helper. `objects()` already excludes
    // deleted rows by default (see Test 3); this pins that the explicit
    // helper narrows a mixed result to the live rows the same way.
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

// ---------------------------------------------------------------------------
// Test 4b — a model that is BOTH `tenant_key` and `soft_deletable` compiles
// (no duplicate `objects_insecurely` — the tenant bypass and the soft-delete
// bypass are distinct names) and the two filters compose independently:
//   - `objects()` excludes deleted (soft-delete default filter applied).
//   - `objects_insecurely()` (tenant bypass) STILL excludes deleted — it
//     drops the RLS predicate, not the soft-delete leaf.
//   - `objects_including_deleted()` (soft-delete bypass) includes deleted.
// ---------------------------------------------------------------------------

#[model(table = "tenant_soft_posts", pk = HeerId, tenant_key = "org_id", soft_deletable)]
#[derive(Debug, Clone)]
pub struct TenantSoft {
    pub org_id: String,
    pub title: String,
    pub deleted_at: Option<djogi::DateTime>,
}

#[djogi::djogi_test(sync_models = [TenantSoft])]
async fn tenant_and_soft_deletable_compose_independently(mut ctx: djogi::DjogiContext) {
    let mut tx = ctx.begin().await.expect("begin transaction");
    tx.set_auth(AuthContext::new(HeerId::from_i64(1).unwrap()).with_tenant("org_a"));

    TenantSoft::create(
        &mut tx,
        TenantSoft {
            org_id: "org_a".to_string(),
            title: "live".to_string(),
            deleted_at: None,
            ..Default::default()
        },
    )
    .await
    .expect("create live tenant-scoped row");

    TenantSoft::create(
        &mut tx,
        TenantSoft {
            org_id: "org_a".to_string(),
            title: "trashed".to_string(),
            deleted_at: Some(OffsetDateTime::now_utc()),
            ..Default::default()
        },
    )
    .await
    .expect("create trashed tenant-scoped row");

    // objects() — soft-delete default filter applied → only the live row.
    let default_rows = TenantSoft::objects()
        .fetch_all(&mut tx)
        .await
        .expect("tenant-scoped default fetch");
    assert_eq!(
        default_rows.len(),
        1,
        "objects() on a tenant + soft-deletable model must exclude the deleted row",
    );
    assert_eq!(default_rows[0].title, "live");

    // objects_insecurely() — bypasses the RLS tenant predicate, NOT the
    // soft-delete leaf. The soft-delete filter lives in the queryset
    // condition (orthogonal to the Postgres policy), so the deleted row
    // stays excluded.
    let insecure_rows = TenantSoft::objects_insecurely()
        .fetch_all(&mut tx)
        .await
        .expect("tenant-bypass fetch");
    assert_eq!(
        insecure_rows.len(),
        1,
        "objects_insecurely() bypasses RLS only — the soft-delete default filter \
         must still exclude the deleted row",
    );
    assert_eq!(insecure_rows[0].title, "live");

    // objects_including_deleted() — bypasses the soft-delete leaf → both rows.
    let including_rows = TenantSoft::objects_including_deleted()
        .fetch_all(&mut tx)
        .await
        .expect("soft-delete-bypass fetch");
    assert_eq!(
        including_rows.len(),
        2,
        "objects_including_deleted() must include the deleted row; the tenant and \
         soft-delete bypasses are orthogonal and compose without an E0592 collision",
    );

    tx.commit().await.expect("commit transaction");
}

// ---------------------------------------------------------------------------
// Test 5 — a proxy model that is ALSO soft-deletable gets both the proxy
// filter and the soft-delete filter in objects().
// ---------------------------------------------------------------------------

#[model(table = "soft_proxy_parent", soft_deletable)]
#[derive(Debug, Clone)]
pub struct SoftProxyParent {
    pub note: String,
    pub active: bool,
    pub deleted_at: Option<djogi::DateTime>,
}

#[model(
    table = "soft_proxy_parent",
    proxy_for = SoftProxyParent,
    default_filter = |f| f.active.eq(true),
    soft_deletable
)]
#[derive(Debug, Clone)]
pub struct ActiveSoftProxy {
    pub note: String,
    pub active: bool,
    pub deleted_at: Option<djogi::DateTime>,
}

#[djogi::djogi_test(sync_models = [SoftProxyParent])]
async fn soft_deletable_proxy_applies_both_filters(mut ctx: djogi::DjogiContext) {
    // active + live  -> survives both filters
    SoftProxyParent::create(
        &mut ctx,
        SoftProxyParent {
            note: "keep".into(),
            active: true,
            deleted_at: None,
            ..Default::default()
        },
    )
    .await
    .expect("create active live row");

    // active + trashed -> dropped by soft-delete filter
    SoftProxyParent::create(
        &mut ctx,
        SoftProxyParent {
            note: "trashed".into(),
            active: true,
            deleted_at: Some(OffsetDateTime::now_utc()),
            ..Default::default()
        },
    )
    .await
    .expect("create active trashed row");

    // inactive + live -> dropped by proxy filter
    SoftProxyParent::create(
        &mut ctx,
        SoftProxyParent {
            note: "inactive".into(),
            active: false,
            deleted_at: None,
            ..Default::default()
        },
    )
    .await
    .expect("create inactive live row");

    let rows = ActiveSoftProxy::objects()
        .fetch_all(&mut ctx)
        .await
        .expect("fetch_all should succeed");

    assert_eq!(
        rows.len(),
        1,
        "a soft-deletable proxy must apply BOTH the proxy default_filter (active = true) \
         and the soft-delete default filter (deleted_at IS NULL)",
    );
    assert_eq!(
        rows[0].note, "keep",
        "only the active live row survives both filters"
    );
}

// ---------------------------------------------------------------------------
// Test 5b — objects_including_deleted() on a proxy + soft_deletable model
// bypasses the soft-delete filter but PRESERVES the proxy filter.
//   active + live    -> returned (in proxy scope, not deleted)
//   active + trashed -> returned (in proxy scope, soft-delete bypassed)
//   inactive + live  -> NOT returned (proxy filter still excludes it)
// A proxy-scoped-out row leaking through the soft-delete bypass would be a
// data exposure — this is the regression guard against it.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [SoftProxyParent])]
async fn soft_deletable_proxy_bypass_preserves_proxy_filter(mut ctx: djogi::DjogiContext) {
    // active + live -> in proxy scope, not deleted
    SoftProxyParent::create(
        &mut ctx,
        SoftProxyParent {
            note: "keep".into(),
            active: true,
            deleted_at: None,
            ..Default::default()
        },
    )
    .await
    .expect("create active live row");

    // active + trashed -> in proxy scope, soft-deleted
    SoftProxyParent::create(
        &mut ctx,
        SoftProxyParent {
            note: "trashed".into(),
            active: true,
            deleted_at: Some(OffsetDateTime::now_utc()),
            ..Default::default()
        },
    )
    .await
    .expect("create active trashed row");

    // inactive + live -> OUT of proxy scope
    SoftProxyParent::create(
        &mut ctx,
        SoftProxyParent {
            note: "inactive".into(),
            active: false,
            deleted_at: None,
            ..Default::default()
        },
    )
    .await
    .expect("create inactive live row");

    let rows = ActiveSoftProxy::objects_including_deleted()
        .fetch_all(&mut ctx)
        .await
        .expect("fetch_all should succeed");

    // Both active rows (live + trashed) come back — soft-delete is bypassed.
    assert_eq!(
        rows.len(),
        2,
        "objects_including_deleted() on a soft-deletable proxy must include the \
         soft-deleted in-scope row (soft-delete bypassed) AND the live in-scope row, \
         but NOT the proxy-excluded row (proxy filter preserved)",
    );
    let mut notes: Vec<&str> = rows.iter().map(|r| r.note.as_str()).collect();
    notes.sort_unstable();
    assert_eq!(
        notes,
        vec!["keep", "trashed"],
        "the proxy-excluded 'inactive' row must NOT leak through the soft-delete bypass",
    );
    assert!(
        !notes.contains(&"inactive"),
        "a proxy-scoped-out row must stay hidden even when including deleted rows",
    );
}

// ---------------------------------------------------------------------------
// Test 6 — a visage over a soft-deletable model excludes deleted rows by
// default (the visage queryset seeds the source model's default filter).
// ---------------------------------------------------------------------------

#[model(table = "soft_visage", soft_deletable)]
#[derive(Debug, Clone)]
pub struct SoftVisage {
    #[field(expose(public))]
    pub note: String,
    pub deleted_at: Option<djogi::DateTime>,
}

#[djogi::djogi_test(sync_models = [SoftVisage])]
async fn soft_deletable_visage_excludes_deleted_by_default(mut ctx: djogi::DjogiContext) {
    SoftVisage::create(
        &mut ctx,
        SoftVisage {
            note: "shared".into(),
            deleted_at: None,
            ..Default::default()
        },
    )
    .await
    .expect("create live row");

    SoftVisage::create(
        &mut ctx,
        SoftVisage {
            note: "shared".into(),
            deleted_at: Some(OffsetDateTime::now_utc()),
            ..Default::default()
        },
    )
    .await
    .expect("create trashed row");

    // The visage predicate matches BOTH rows by note; only the soft-delete
    // default filter (inherited from the source model) excludes the trashed
    // one, isolating the default-filter behavior.
    let rows = SoftVisagePublic::filter(|v| v.note().eq("shared".to_string()))
        .fetch_all(&mut ctx)
        .await
        .expect("visage fetch_all should succeed");

    assert_eq!(
        rows.len(),
        1,
        "a visage over a soft-deletable model must inherit the deleted_at IS NULL default filter",
    );
}

// ---------------------------------------------------------------------------
// Test 7 — prefetch of a soft-deletable child excludes deleted children.
// A deleted child surfaces as a LEFT JOIN miss (None), identical to a
// NULL FK or orphan target.
// ---------------------------------------------------------------------------

#[model(table = "soft_child", soft_deletable)]
#[derive(Debug, Clone)]
pub struct SoftChild {
    pub label: String,
    pub deleted_at: Option<djogi::DateTime>,
}

// `no_default` is required because `ForeignKey<T>` intentionally does not
// implement `Default` — a relation with no PK value is meaningless. Rows are
// built with explicit framework-field sentinels (the DB overwrites them via
// `RETURNING *` on insert) through `soft_parent_for_insert`.
#[model(table = "soft_parent", no_default)]
#[derive(Debug, Clone)]
pub struct SoftParent {
    pub name: String,
    pub child_id: ForeignKey<SoftChild>,
}

/// Build a `SoftParent` value for `SoftParent::create`. Framework fields use
/// the recency-biased PK sentinel + epoch timestamps; the DB defaults
/// overwrite them via `RETURNING *`. Extracted because `no_default` forbids
/// `..Default::default()`.
fn soft_parent_for_insert(name: &str, child: &SoftChild) -> SoftParent {
    SoftParent {
        id: <HeerIdRecencyBiased as PrimaryKey>::sentinel(),
        created_at: djogi::DateTime::UNIX_EPOCH,
        updated_at: djogi::DateTime::UNIX_EPOCH,
        name: name.into(),
        child_id: ForeignKey::new(child.id),
    }
}

#[djogi::djogi_test(sync_models = [SoftChild, SoftParent])]
async fn prefetch_soft_deletable_child_excludes_deleted(mut ctx: djogi::DjogiContext) {
    let live_child = SoftChild::create(
        &mut ctx,
        SoftChild {
            label: "live".into(),
            deleted_at: None,
            ..Default::default()
        },
    )
    .await
    .expect("create live child");

    let dead_child = SoftChild::create(
        &mut ctx,
        SoftChild {
            label: "dead".into(),
            deleted_at: Some(OffsetDateTime::now_utc()),
            ..Default::default()
        },
    )
    .await
    .expect("create dead child");

    SoftParent::create(&mut ctx, soft_parent_for_insert("p_live", &live_child))
        .await
        .expect("create parent pointing at live child");

    SoftParent::create(&mut ctx, soft_parent_for_insert("p_dead", &dead_child))
        .await
        .expect("create parent pointing at dead child");

    let parents = SoftParent::objects()
        .prefetch(SoftParentRelated::child())
        .fetch_all_prefetched(&mut ctx)
        .await
        .expect("prefetch fetch should succeed");

    let p_live = parents
        .iter()
        .find(|p| p.row.name == "p_live")
        .expect("p_live present");
    let p_dead = parents
        .iter()
        .find(|p| p.row.name == "p_dead")
        .expect("p_dead present");

    assert!(
        p_live.get(SoftParentRelated::child()).is_some(),
        "parent pointing at a live child must resolve the prefetched child",
    );
    assert!(
        p_dead.get(SoftParentRelated::child()).is_none(),
        "parent pointing at a deleted child must see the prefetched child as None",
    );
}

// ---------------------------------------------------------------------------
// Test 8 — select_related of a soft-deletable child excludes deleted
// children (deleted child decodes to None via LEFT JOIN miss). Reuses the
// SoftChild / SoftParent fixtures from Test 7.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [SoftChild, SoftParent])]
async fn select_related_soft_deletable_child_excludes_deleted(mut ctx: djogi::DjogiContext) {
    let live_child = SoftChild::create(
        &mut ctx,
        SoftChild {
            label: "live".into(),
            deleted_at: None,
            ..Default::default()
        },
    )
    .await
    .expect("create live child");

    let dead_child = SoftChild::create(
        &mut ctx,
        SoftChild {
            label: "dead".into(),
            deleted_at: Some(OffsetDateTime::now_utc()),
            ..Default::default()
        },
    )
    .await
    .expect("create dead child");

    SoftParent::create(&mut ctx, soft_parent_for_insert("sr_live", &live_child))
        .await
        .expect("create parent pointing at live child");

    SoftParent::create(&mut ctx, soft_parent_for_insert("sr_dead", &dead_child))
        .await
        .expect("create parent pointing at dead child");

    let rows = SoftParent::objects()
        .select_related(SoftParentRelated::child())
        .fetch_all_joined(&mut ctx)
        .await
        .expect("select_related fetch should succeed");

    let live_joined = rows
        .iter()
        .find(|j| j.row.name == "sr_live")
        .expect("sr_live present");
    let dead_joined = rows
        .iter()
        .find(|j| j.row.name == "sr_dead")
        .expect("sr_dead present");

    assert!(
        live_joined.get(SoftParentRelated::child()).is_some(),
        "parent pointing at a live child must resolve the select_related child",
    );
    assert!(
        dead_joined.get(SoftParentRelated::child()).is_none(),
        "parent pointing at a deleted child must see the select_related child as None",
    );
}

// ---------------------------------------------------------------------------
// Test 9 — in_bulk() honours the soft-delete default filter. A deleted row
// requested by PK must NOT come back from objects().in_bulk(...). Reuses the
// SoftDefault model from Test 3.
// ---------------------------------------------------------------------------
//
// Test 9b — in_bulk() with a value-bearing filter condition verifies that
// condition parameters are bound BEFORE the IN-list parameters. If the bind
// order were reversed, a string value in `$1` would be compared against the id
// column (or vice-versa), producing wrong results or a type error. Using a
// string equality predicate (note = $1) guarantees at least one named
// parameter precedes the PK IN binds ($2, $3, $4), so a regression in
// parameter ordering is directly observable as wrong rows returned.
//
// Reuses the SoftDefault model from Test 3 (table `soft_default`).
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [SoftDefault])]
async fn in_bulk_respects_soft_delete_default_filter(mut ctx: djogi::DjogiContext) {
    let live = SoftDefault::create(
        &mut ctx,
        SoftDefault {
            note: "live".into(),
            deleted_at: None,
            ..Default::default()
        },
    )
    .await
    .expect("create live row");

    let trashed = SoftDefault::create(
        &mut ctx,
        SoftDefault {
            note: "trashed".into(),
            deleted_at: Some(OffsetDateTime::now_utc()),
            ..Default::default()
        },
    )
    .await
    .expect("create trashed row");

    let found = SoftDefault::objects()
        .in_bulk(&mut ctx, vec![live.id, trashed.id])
        .await
        .expect("in_bulk should succeed");

    assert!(
        found.contains_key(&live.id),
        "in_bulk must return the live row requested by PK",
    );
    assert!(
        !found.contains_key(&trashed.id),
        "in_bulk must NOT return a soft-deleted row — the objects() default filter \
         (deleted_at IS NULL) must compose with the PK IN predicate",
    );
    assert_eq!(
        found.len(),
        1,
        "exactly the live row survives the default filter"
    );
}

#[djogi::djogi_test(sync_models = [SoftDefault])]
async fn in_bulk_honours_value_bearing_condition_and_default_filter(mut ctx: djogi::DjogiContext) {
    // Row A: matches the note filter and is not deleted — must appear in result.
    let row_a = SoftDefault::create(
        &mut ctx,
        SoftDefault {
            note: "live".into(),
            deleted_at: None,
            ..Default::default()
        },
    )
    .await
    .expect("create row_a should succeed");

    // Row B: different note — excluded by the string equality predicate.
    let row_b = SoftDefault::create(
        &mut ctx,
        SoftDefault {
            note: "other".into(),
            deleted_at: None,
            ..Default::default()
        },
    )
    .await
    .expect("create row_b should succeed");

    // Row C: matches the note filter but is soft-deleted — excluded by the
    // soft-delete default filter that objects() seeds automatically.
    let row_c = SoftDefault::create(
        &mut ctx,
        SoftDefault {
            note: "live".into(),
            deleted_at: Some(time::OffsetDateTime::now_utc()),
            ..Default::default()
        },
    )
    .await
    .expect("create row_c should succeed");

    // The filter binds `note = $1` before the PK IN list binds `$2, $3, $4`.
    // If the accumulator pushed IN binds first, the string value would be sent
    // as `$1` while the id placeholder `$2` would try to compare an integer to
    // the note column — causing wrong results or a Postgres type error.
    let found = SoftDefault::objects()
        .filter(|f| f.note().eq("live".to_string()))
        .in_bulk(&mut ctx, vec![row_a.id, row_b.id, row_c.id])
        .await
        .expect("in_bulk with value-bearing condition should succeed");

    assert!(
        found.contains_key(&row_a.id),
        "row_a (note = 'live', not deleted) must be returned by in_bulk",
    );
    assert!(
        !found.contains_key(&row_b.id),
        "row_b (note = 'other') must be excluded by the note equality predicate",
    );
    assert!(
        !found.contains_key(&row_c.id),
        "row_c (note = 'live', deleted) must be excluded by the soft-delete default filter",
    );
    assert_eq!(
        found.len(),
        1,
        "exactly one row survives both the value-bearing condition and the soft-delete filter",
    );
    assert_eq!(
        found[&row_a.id].note, "live",
        "the surviving row must be the one with note = 'live' that is not deleted",
    );
}

// ---------------------------------------------------------------------------
// Portability-gate rejection — no DB required
//
// The automatic `deleted_at IS NULL` condition is seeded into
// `QuerySet::new()` as `Q::Condition(c)`, which is intentionally
// non-portable. `try_portable()` calls `try_reduce_q_ref_to_basic` on the
// condition tree; that function rejects `Q::Condition` with
// `PortablePredicateError`. As a consequence, `SoftDeletableModel::objects()`
// can never reach the `refresh_into` delta-refresh path — which is correct:
// the delta-refresh SQL fetcher builds its own SQL from scratch and must NOT
// inherit the IS NULL predicate. Deleted rows must flow through the watermark
// for tombstone collection; filtering them out at the queryset level would
// cause them to be silently ignored rather than collected as tombstones.
// ---------------------------------------------------------------------------

#[test]
fn soft_delete_default_filter_is_non_portable_blocking_refresh_into() {
    let qs = SoftDefault::objects();
    // The automatic IS NULL default filter is seeded as Q::Condition, which is
    // intentionally non-portable. Any attempt to call refresh_into on
    // objects() therefore returns Err at the portability gate — the
    // delta-refresh SQL fetcher must not inherit the IS NULL predicate
    // (deleted rows flow through the watermark for tombstone collection,
    // not via IS NULL filtering).
    assert!(
        qs.try_portable().is_err(),
        "SoftDefault::objects() must be non-portable because the soft-delete \
         IS NULL condition is Q::Condition, which blocks refresh_into",
    );
}
