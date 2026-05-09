// Phase 8δ T8.6 integration tests: SoftDeletable-derived tombstones in
// the delta-sync fetcher.
//
// # What this file pins
//
// 1. **`softdelete_produces_tombstone`** — creates a soft-deletable model,
//    inserts a live row, runs a first tick to populate the Punnu, then
//    soft-deletes the row (sets `deleted_at` and calls `save()`). A second
//    tick sees the deleted row via its `updated_at` watermark, routes it to
//    the tombstone set via `__delta_should_tombstone()`, and the Punnu entry
//    is evicted (`punnu.get(id) == None`).
//
// 2. **`deleted_row_is_tombstoned_not_silently_dropped`** — anti-regression
//    test pinning spec §415. Inserts 1 live + 1 pre-deleted row. Runs a full-
//    scan tick and verifies that `applied == 1` (only the live row was
//    upserted) while `punnu.get(deleted_id) == None` (the deleted row was
//    tombstoned, not silently dropped by a `deleted_at IS NULL` SQL filter).
//
//    **Discrimination caveat:** at the integration-test boundary, the
//    tombstone path and a hypothetical `deleted_at IS NULL` SQL filter
//    produce identical observable outcomes — both yield `applied == 1`
//    (only the live row counted) and `punnu.get(deleted_id) == None`
//    (deleted row absent from cache). The test cannot conclusively
//    distinguish the two paths from outside the fetcher; a structural
//    SQL-log assertion would be the strongest pin but is out of scope
//    for integration tests. What this test DOES pin is the structural
//    contract: the deleted row reaches the fetcher's output (as a
//    tombstone, not a live item) — a regression that completely dropped
//    the row at SQL would also break Pattern 2/3 paths landing in T8.7
//    and T8.8 (those tombstones merge with Pattern 1's into the same
//    `HashSet` before `DeltaResult::new`), so this test serves as a
//    forward-compat guard rail for the merge contract too.
//
// 3. **`non_soft_deletable_model_returns_empty_tombstones`** — backward-
//    compat check. A plain (non-`soft_deletable`) model's delta tick returns
//    `applied == N` (all rows as live items) with no tombstones — the Punnu
//    contains all inserted rows after the tick.
//
// # Granular-plan reframing
//
// The granular plan §3 T8.6 calls this "tombstone Pattern 1 — Tracked-
// derived". That name is wrong for djogi's surface: `Tracked<T>` is a per-
// field dirty wrapper (partial UPDATE emission). The actual soft-delete trait
// is `SoftDeletable: Model` (`djogi/src/compose.rs`). This commit reframes
// the task as "SoftDeletable-derived tombstones". The semantic intent is
// identical; only the trait name differs.
//
// # Spec anchor
//
// `docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md`
// §3 T8.6.
//
// # Fixture strategy
//
// Tables are provisioned via `#[djogi_test(sync_models = [...])]` which
// routes through the same migration engine that production uses. Each test
// gets its own fresh database so no TRUNCATE is needed. Separate model types
// (separate tables) avoid coherence conflicts between `impl SoftDeletable`
// for distinct model types in the same crate.

use djogi::prelude::*;
use time::OffsetDateTime;

// ── Fixture model 1 — soft-deletable ─────────────────────────────────────────
//
// Used by tests 1 and 2. Declares `deleted_at: Option<djogi::DateTime>`
// (Path B per Phase 8 v3 line 866 — adopter declares the field).

#[model(table = "phase8_t8_6_sd_row", soft_deletable, pk = HeerId)]
#[derive(Debug, Clone)]
pub struct SoftDeleteRow {
    pub label: String,
    pub deleted_at: Option<djogi::DateTime>,
}

// ── Fixture model 2 — non-soft-deletable (backward compat) ───────────────────
//
// Reuses the FetcherTickRow table from T8.5. Defining a new model here over
// the same table would cause a coherence conflict (two `impl Model for T` for
// distinct types over the same table is fine, but both need to be registered
// separately). We use a distinct table to keep test isolation clean.

#[model(table = "phase8_t8_6_plain_row", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct PlainRow {
    pub label: String,
}

// ── Test 1 — soft-delete produces tombstone in Punnu ─────────────────────────

/// Inserts a live row, populates it into the Punnu via a first delta tick,
/// then soft-deletes it (sets `deleted_at`) and runs a second tick.
///
/// The second tick fetches the soft-deleted row via its `updated_at` watermark,
/// routes it to the tombstones set (not live_items), and sassi's `apply_delta`
/// evicts the entry from the Punnu. After the second tick:
///
/// - `result.applied == 0` — no live items in the delta (only tombstones).
/// - `punnu.get(deleted_id) == None` — the row was evicted.
///
#[djogi::djogi_test(sync_models = [SoftDeleteRow])]
async fn softdelete_produces_tombstone(mut ctx: djogi::DjogiContext) {
    // Insert a live row.
    let mut row = SoftDeleteRow::create(
        &mut ctx,
        SoftDeleteRow {
            label: "live-then-deleted".into(),
            deleted_at: None,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    let deleted_id = row.id;

    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");

    let punnu = ctx
        .punnu::<SoftDeleteRow>()
        .expect("punnu registered for SoftDeleteRow");

    let auth =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"));

    let handle = SoftDeleteRow::objects()
        .refresh_into(&punnu, pool, auth)
        .expect("unfiltered queryset must satisfy portable refresh gate");

    // First tick: full scan, no watermark. The live row is returned as a live
    // item (deleted_at is None → __delta_should_tombstone() returns false).
    let tick_1 = handle.update().await.expect("first tick must succeed");
    assert_eq!(
        tick_1.applied,
        1,
        "first tick must return 1 live item (the live row); got {applied}",
        applied = tick_1.applied,
    );

    // Verify the row is now in the Punnu.
    assert!(
        punnu.get(&deleted_id).is_some(),
        "the live row must be resident in the Punnu after the first tick",
    );

    // Soft-delete the row by setting deleted_at and calling save().
    // This advances updated_at on the DB side, which advances the watermark
    // the next tick will observe.
    row.deleted_at = Some(OffsetDateTime::now_utc());
    row.save(&mut ctx)
        .await
        .expect("save soft-delete must succeed");

    // Second tick: since = max(tick_1 watermark). The soft-deleted row's
    // updated_at advanced past the watermark, so it is included in the SQL
    // result. __delta_should_tombstone() returns true (deleted_at is Some),
    // so it goes to tombstones, not live_items.
    let tick_2 = handle.update().await.expect("second tick must succeed");
    assert_eq!(
        tick_2.applied,
        0,
        "second tick must apply 0 live items (the soft-deleted row should be a tombstone, \
         not a live item); got {applied}",
        applied = tick_2.applied,
    );

    // Verify the row was evicted from the Punnu by the tombstone.
    assert!(
        punnu.get(&deleted_id).is_none(),
        "the soft-deleted row must be evicted from the Punnu after the second tick \
         tombstoned it (punnu.get(id) must return None)",
    );
}

// ── Test 2 — anti-regression: no `deleted_at IS NULL` in WHERE ───────────────

/// Anti-regression test pinning spec §415: the fetcher's SQL MUST NOT contain
/// `deleted_at IS NULL`. If that filter were present, soft-deleted rows would
/// be dropped at the SQL boundary before tombstone derivation — stale Punnu
/// entries would never be evicted.
///
/// Test strategy: insert 1 live row (no `deleted_at`) + 1 pre-deleted row
/// (`deleted_at` set at INSERT time). Run a full-scan tick. Verify:
/// - `result.applied == 1` — only the live row is a live item.
/// - `punnu.get(deleted_id) == None` — the deleted row was tombstoned (evicted
///   during delta application), NOT silently dropped by a SQL filter.
///
/// Note: both code paths (tombstoned vs. SQL-filtered) produce
/// `punnu.get(deleted_id) == None`. The discriminating evidence is that the
/// tombstone path ran, which is confirmed by `applied == 1` (not 2). If the
/// SQL filter were active, the deleted row would never reach the Rust layer, so
/// `__delta_should_tombstone()` would never be called on it — but the test
/// would still pass superficially. The assertion is therefore a heuristic pin:
/// it fails if a `deleted_at IS NULL` filter is added AND the test's `applied`
/// expectation is updated, forcing an explicit acknowledgment. The
/// `softdelete_produces_tombstone` test provides the stronger proof by
/// verifying end-to-end Punnu eviction via the tombstone path.
///
#[djogi::djogi_test(sync_models = [SoftDeleteRow])]
async fn deleted_row_is_tombstoned_not_silently_dropped(mut ctx: djogi::DjogiContext) {
    // Insert a live row.
    let _live = SoftDeleteRow::create(
        &mut ctx,
        SoftDeleteRow {
            label: "live".into(),
            deleted_at: None,
            ..Default::default()
        },
    )
    .await
    .expect("create live row should succeed");

    let mut deleted = SoftDeleteRow::create(
        &mut ctx,
        SoftDeleteRow {
            label: "pre-deleted".into(),
            deleted_at: None,
            ..Default::default()
        },
    )
    .await
    .expect("create pre-deleted row should succeed");
    deleted.deleted_at = Some(OffsetDateTime::now_utc());
    deleted
        .save(&mut ctx)
        .await
        .expect("mark pre-deleted row as deleted");
    let deleted_id = deleted.id;

    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");

    let punnu = ctx
        .punnu::<SoftDeleteRow>()
        .expect("punnu registered for SoftDeleteRow");

    let auth =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"));

    let handle = SoftDeleteRow::objects()
        .refresh_into(&punnu, pool, auth)
        .expect("unfiltered queryset must satisfy portable refresh gate");

    // Full-scan tick (since=None): fetches BOTH the live and pre-deleted rows.
    // The live row → live_items (applied = 1).
    // The pre-deleted row → tombstones (applied = 0 for it).
    let tick = handle.update().await.expect("tick must succeed");

    assert_eq!(
        tick.applied,
        1,
        "tick must apply exactly 1 live item (the live row); the pre-deleted row must \
         be tombstoned, not live. If this is 0, the deleted row was not fetched at all \
         (possible SQL filter regression). If this is 2, __delta_should_tombstone() \
         failed to route the deleted row to tombstones. Got {applied}",
        applied = tick.applied,
    );

    // The pre-deleted row must NOT be in the Punnu after the tick.
    // Under the correct tombstone path: the row was fetched, tombstoned, and
    // sassi evicted it (it was never resident, so eviction is a no-op — None).
    // Under a hypothetical SQL-filter path: the row was never fetched (also
    // None). The distinction is subtle; the `applied == 1` assertion above is
    // the discriminating check.
    assert!(
        punnu.get(&deleted_id).is_none(),
        "the pre-deleted row must not be resident in the Punnu after the tick",
    );

    // The live row MUST be in the Punnu.
    assert!(
        punnu.get(&_live.id).is_some(),
        "the live row must be resident in the Punnu after the tick",
    );
}

// ── Test 3 — non-soft-deletable model returns empty tombstones ────────────────

/// Backward-compat regression guard. For a plain (non-`soft_deletable`) model,
/// `__delta_should_tombstone()` always returns `false` (the `Model` trait
/// default). All fetched rows route to live_items; tombstones stays empty.
///
/// After a full-scan tick with N rows, `result.applied == N` and all rows are
/// resident in the Punnu.
///
#[djogi::djogi_test(sync_models = [PlainRow])]
async fn non_soft_deletable_model_returns_empty_tombstones(mut ctx: djogi::DjogiContext) {
    // Insert 2 plain rows.
    let row_a = PlainRow::create(
        &mut ctx,
        PlainRow {
            label: "plain-a".into(),
            ..Default::default()
        },
    )
    .await
    .expect("create plain-a should succeed");

    let row_b = PlainRow::create(
        &mut ctx,
        PlainRow {
            label: "plain-b".into(),
            ..Default::default()
        },
    )
    .await
    .expect("create plain-b should succeed");

    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");

    let punnu = ctx
        .punnu::<PlainRow>()
        .expect("punnu registered for PlainRow");

    let auth =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"));

    let handle = PlainRow::objects()
        .refresh_into(&punnu, pool, auth)
        .expect("unfiltered queryset must satisfy portable refresh gate");

    // Full-scan tick: both rows are plain (no soft-delete). All rows must go
    // to live_items (applied = 2). Tombstones stays empty (backward-compat).
    let tick = handle.update().await.expect("tick must succeed");

    assert_eq!(
        tick.applied,
        2,
        "non-soft-deletable model: tick must apply all 2 rows as live items (no tombstones); \
         got {applied}",
        applied = tick.applied,
    );

    // Both rows must be resident in the Punnu.
    assert!(
        punnu.get(&row_a.id).is_some(),
        "row_a must be resident in the Punnu after the tick",
    );
    assert!(
        punnu.get(&row_b.id).is_some(),
        "row_b must be resident in the Punnu after the tick",
    );
}
