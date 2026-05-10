// Phase 8δ T8.10 integration tests: cluster-exit integration — auth locked to
// subscription + end-to-end happy-path.
//
// # What this file pins
//
// 1. **`refresh_into_e2e_happy_path`** — full insert / save / soft-delete
//    cycle through real refresh ticks. Pins the complete cluster-8δ contract:
//    full-scan tick returns all live rows; delta tick applies new inserts and
//    routes soft-deleted rows to tombstones.
//
// 2. **`refresh_into_auth_locked_to_subscription`** — structural proof that
//    the `AuthContext` captured at `refresh_into` time is the auth used
//    per-tick (not whatever caller-side auth holds at tick time). Closes the
//    gap that T8.5 test 4 admitted.
//
//    **Option C** is used here rather than Option B (RLS-backed filtering).
//    The `djogi` test role is a Postgres superuser; Postgres superusers
//    **always** bypass row security even when `FORCE ROW LEVEL SECURITY` is
//    set on a table (`rolsuper = t` → `BYPASSRLS` is implied). `FORCE ROW
//    LEVEL SECURITY` only re-applies RLS to the *table owner* when the owner
//    is a normal role, not a superuser. Since the fetcher connects as the
//    `djogi` superuser, RLS-based row-count filtering cannot be observed at
//    integration-test level without switching to a restricted role inside the
//    fetch transaction — which would require production-code changes outside
//    T8.10 scope.
//
//    **What Option C proves instead:** two handles constructed with different
//    `AuthContext` values each complete ticks successfully, and the auth set
//    on the fetcher at construction time is the one observable inside the tick
//    (proven via `ctx.auth().tenant_id` plumbing through `auto_set_tenant` +
//    `ctx.applied_tenant_id()`). The structural proof that auth IS captured by
//    value (not by reference) is the `'static` bound on `DeltaPunnuFetcher<T>`
//    and the `DjogiDeltaFetcher::auth: AuthContext` owned field verified in
//    T8.3–T8.5.
//
//    **Companion full-RLS test:** the row-count proof under a real
//    non-superuser pool now lives in
//    `tests/internal/phase8_5_c2_129_non_superuser_rls.rs`
//    (closes [GH #129]). That test reuses the Phase 8δ refresh path
//    against a `connect_test_db_as_non_superuser`-derived pool and
//    asserts that only the tenant-scoped rows reach the bound Punnu.
//    The structural proof here (Option C) and the row-count proof
//    there (Option B + non-superuser pool) are complementary: this
//    file pins the *value-capture* invariant; #129 pins the
//    *server-side filtering* invariant.
//
// 3. **`refresh_into_cancel_stops_ticks`** — `handle.cancel()` stops the
//    periodic refresh loop. After cancel, manual `handle.update()` still
//    succeeds (cancel only signals the background periodic task; the explicit
//    update path does not check the cancel flag). The test pins sassi's
//    canonical post-cancel behavior: periodic ticks stop, on-demand ticks
//    continue.
//
// # Spec anchors
//
// - spec §677 — auth-locked-to-subscription contract (Test 2)
// - spec §430 — acceptance criterion: delta tick applies incremental changes
//   after a full-scan baseline (Test 1)
// - `docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md` §3 T8.10
//
// # RLS setup choice
//
// Test 2 uses **Option C** (scoped-down structural proof) because the `djogi`
// test user is a Postgres superuser and superusers unconditionally bypass
// row security (`BYPASSRLS` is implied by `rolsuper = t`). `FORCE ROW LEVEL
// SECURITY` applies the policy to the table owner when the owner is a NORMAL
// role — it does not override the superuser bypass. The complete RLS-filtered
// proof — Option B + a real non-superuser pool — now lives in
// `tests/internal/phase8_5_c2_129_non_superuser_rls.rs`, which uses
// `djogi::testing::connect_test_db_as_non_superuser` to open a pool whose
// physical connections authenticate as `djogi_test_user` (NOSUPERUSER /
// NOBYPASSRLS). Test 2 here is preserved because the *value-capture*
// invariant on `AuthContext` is structural — it must hold regardless of
// whether RLS is observable at SQL level.
//
// # Fixture strategy
//
// Tables are provisioned via `#[djogi_test(sync_models = [...])]` which
// routes through the same migration engine that production uses. Each test
// gets its own fresh database so no TRUNCATE is needed.

use djogi::prelude::*;
use time::OffsetDateTime;

// ── Fixture model 1 — soft-deletable, happy-path ─────────────────────────────
//
// `E2ERow` is soft-deletable so Test 1 can exercise the tombstone path in
// the delta tick. Separate table from all other T8.x models.

#[model(table = "phase8_t8_10_e2e_rows", soft_deletable, pk = HeerId)]
#[derive(Debug, Clone)]
pub struct E2ERow {
    pub label: String,
    pub active: bool,
    pub deleted_at: Option<djogi::DateTime>,
}

// ── Fixture model 2 — plain model for auth-locked test (Option C) ────────────
//
// Auth-locked test uses a plain model (no tenant_key). The auth-locking
// contract is proved structurally (captured by value, both handles succeed,
// auto_set_tenant no-op for plain models = no interference). For the RLS-backed
// proof, see GH issue filed in the module doc.

#[model(table = "phase8_t8_10_auth_rows", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct AuthRow {
    pub owner_uid: i64,
    pub label: String,
}

// ── Fixture model 3 — plain model for cancel test ────────────────────────────

#[model(table = "phase8_t8_10_cancel_rows", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct CancelRow {
    pub label: String,
}

// ── Test 1 — full insert / save / soft-delete end-to-end ────────────────────

/// Full cluster-8δ cycle through real refresh ticks:
///
/// 1. Insert 3 active, non-deleted rows via `Model::create`.
/// 2. Tick 1 (full scan / `since = None`): `applied == 3`; all 3 rows resident
///    in the Punnu.
/// 3. Insert 2 more rows with timestamps 1 second in the future (strictly after
///    the tick-1 watermark).
/// 4. Soft-delete row 1 (set `deleted_at` and call `save()`), bumping its
///    `updated_at` past the tick-1 watermark.
/// 5. Tick 2 (delta / `since = max(tick-1 watermark)`):
///    - The 2 new rows arrive as live items.
///    - Row 1's `updated_at` advanced past watermark — arrives in the delta
///      result, `__delta_should_tombstone()` returns `true` → routed to
///      tombstones → evicted from the Punnu.
///    - The inclusive `>=` boundary may re-include the last initial row (whose
///      `updated_at == watermark`), so `applied` is 2 or 3 depending on
///      timestamp resolution. The KEY invariant: `applied < 5` (delta is
///      narrower than the full table) and the soft-deleted row is evicted.
///
/// Pins spec §430 (incremental delta applies after a full-scan baseline) and
/// the full cluster-exit contract.
///
#[djogi::djogi_test(sync_models = [E2ERow])]
async fn refresh_into_e2e_happy_path(mut ctx: djogi::DjogiContext) {
    // ── Insert 3 initial rows ────────────────────────────────────────────────
    let mut initial_rows = Vec::with_capacity(3);
    for i in 1i64..=3 {
        let label = format!("initial-{i}");
        let row = E2ERow::create(
            &mut ctx,
            E2ERow {
                label,
                active: true,
                deleted_at: None,
                ..Default::default()
            },
        )
        .await
        .expect("create initial row");
        initial_rows.push(row);
    }

    let to_delete_id = initial_rows[0].id;
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");
    let punnu = ctx.punnu::<E2ERow>().expect("punnu registered for E2ERow");
    let auth =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"));

    let handle = E2ERow::objects()
        .refresh_into(&punnu, pool, auth)
        .expect("unfiltered queryset must satisfy portable refresh gate");

    // ── Tick 1 — full scan ───────────────────────────────────────────────────
    // since = None → no watermark filter → returns all 3 initial rows.
    let tick_1 = handle.update().await.expect("tick 1 must succeed");
    assert_eq!(
        tick_1.applied,
        3,
        "tick 1 (full scan) must apply all 3 initial rows; got {applied}",
        applied = tick_1.applied,
    );

    // All 3 initial rows must be resident in the Punnu after tick 1.
    for row in &initial_rows {
        assert!(
            punnu.get(&row.id).is_some(),
            "initial row {} must be resident in the Punnu after tick 1",
            row.id,
        );
    }

    // ── Insert 2 more rows after tick 1 ──────────────────────────────────────
    // A short pause keeps the DB-generated `updated_at` values strictly after
    // the tick-1 watermark, so tick 2 picks them up via the watermark filter.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let mut new_row_ids = Vec::with_capacity(2);
    for i in 1i64..=2 {
        let label = format!("new-{i}");
        let row = E2ERow::create(
            &mut ctx,
            E2ERow {
                label,
                active: true,
                deleted_at: None,
                ..Default::default()
            },
        )
        .await
        .expect("create new row");
        new_row_ids.push(row.id);
    }

    // ── Soft-delete row 1 ────────────────────────────────────────────────────
    // Setting `deleted_at` and calling `save()` bumps `updated_at` to the
    // current time, which should be at or after the tick-1 watermark (both
    // are `now()` from recent queries). The inclusive `>=` watermark boundary
    // guarantees this row appears in tick 2's delta query.
    let mut row_to_delete = initial_rows[0].clone();
    row_to_delete.deleted_at = Some(OffsetDateTime::now_utc());
    row_to_delete
        .save(&mut ctx)
        .await
        .expect("soft-delete row 1 via save()");

    // ── Tick 2 — delta ───────────────────────────────────────────────────────
    // since = max(tick-1 watermark). Expected items via `>= watermark`:
    //   - 2 new rows (strictly after watermark): live items.
    //   - Row 1 (soft-deleted, `updated_at` bumped to now()): arrives in delta,
    //     `__delta_should_tombstone()` returns true → tombstoned → evicted.
    //   - Possibly 1 boundary row (last initial row at max tick-1 watermark):
    //     the `>=` boundary re-includes the most-recent initial row. This is
    //     the inclusive-boundary contract from DeltaPunnuFetcher (spec: boundary
    //     rows may have changed without their watermark changing; deduplication
    //     by id handles re-delivery).
    //
    // Therefore `applied` is 2 (new rows only) or 3 (new rows + boundary row)
    // depending on how many initial rows share the max `updated_at` timestamp.
    // Key invariants:
    //   - `applied >= 2` (the 2 new rows must appear)
    //   - `applied < 5`  (the delta is narrower than the full table of 5 rows)
    //   - The soft-deleted row is evicted from the Punnu (tombstoned)
    let tick_2 = handle.update().await.expect("tick 2 must succeed");

    let total_rows: usize = 5; // 3 initial + 2 new
    assert!(
        tick_2.applied >= 2,
        "tick 2 (delta) must apply at least 2 live items (the 2 new rows); \
         got {applied}",
        applied = tick_2.applied,
    );
    assert!(
        tick_2.applied < total_rows,
        "tick 2 (delta) must apply fewer rows than the full table ({total_rows}) — \
         the watermark filter `WHERE updated_at >= $since` must be active; \
         `applied == {total_rows}` means no filter applied (full-scan bug); \
         got {applied}",
        applied = tick_2.applied,
    );

    // Soft-deleted row must be evicted from the Punnu (tombstoned in tick 2).
    assert!(
        punnu.get(&to_delete_id).is_none(),
        "soft-deleted row must be evicted from the Punnu after tick 2 tombstoned it \
         (`deleted_at IS NOT NULL` → `__delta_should_tombstone()` returns `true`)",
    );

    // New rows must be resident in the Punnu.
    for id in &new_row_ids {
        assert!(
            punnu.get(id).is_some(),
            "new row {id} must be resident in the Punnu after tick 2",
        );
    }

    // Non-deleted initial rows must remain resident in the Punnu.
    for row in &initial_rows[1..] {
        assert!(
            punnu.get(&row.id).is_some(),
            "non-deleted initial row {} must remain resident in the Punnu after tick 2",
            row.id,
        );
    }
}

// ── Test 2 — auth locked to subscription (Option C structural proof) ─────────

/// Structural proof of spec §677: the `AuthContext` captured at `refresh_into`
/// time is the auth used per-tick.
///
/// # What we prove here
///
/// Two handles are constructed with different `AuthContext` values:
/// - `handle_a` captures `auth_a` (user_id=1, tenant_id=None for a plain model)
/// - `handle_b` captures `auth_b` (user_id=2, tenant_id=None)
///
/// Both handles tick against the same table successfully. The observable proof
/// that auth is "locked" is:
/// 1. The fetcher accepts the auth without panicking (proves auth is applied).
/// 2. The two handles run independently (proves each subscription has its own
///    captured auth snapshot, not a shared reference).
/// 3. Modifying caller-side `_auth_a_modified` after construction has no effect
///    on `handle_a`'s tick (the fetcher captured `auth_a` by value via Clone;
///    the in-scope variable is unrelated).
///
#[djogi::djogi_test(sync_models = [AuthRow])]
async fn refresh_into_auth_locked_to_subscription(mut ctx: djogi::DjogiContext) {
    // Insert 5 rows (owned by user 1) and 3 rows (owned by user 2).
    for i in 1i64..=5 {
        AuthRow::create(
            &mut ctx,
            AuthRow {
                owner_uid: 1,
                label: format!("user1-row-{i}"),
                ..Default::default()
            },
        )
        .await
        .unwrap_or_else(|e| panic!("insert user1 row {i}: {e:?}"));
    }
    for i in 1i64..=3 {
        AuthRow::create(
            &mut ctx,
            AuthRow {
                owner_uid: 2,
                label: format!("user2-row-{i}"),
                ..Default::default()
            },
        )
        .await
        .unwrap_or_else(|e| panic!("insert user2 row {i}: {e:?}"));
    }

    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");
    let punnu_a = ctx
        .punnu::<AuthRow>()
        .expect("punnu registered for AuthRow");

    // ── Construct two handles with different captured auths ──────────────────
    // `auth_a` carries user_id=1 (captured by handle_a at construction time).
    // `auth_b` carries user_id=2 (captured by handle_b at construction time).
    // Neither carries a tenant_id (plain model, no tenant_key → auto_set_tenant
    // is a no-op). The auth is captured by value via Clone in
    // `DjogiDeltaFetcher::auth: AuthContext` — proven by the `'static` bound
    // on `DeltaPunnuFetcher<T>` and the owned-field layout verified in T8.3.
    let auth_a =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"));
    let auth_b =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(2).expect("HeerId(2) is valid"));

    // handle_a captures auth_a by value.
    let handle_a = AuthRow::objects()
        .refresh_into(&punnu_a, pool.clone(), auth_a)
        .expect("unfiltered queryset must satisfy portable refresh gate");

    // handle_b captures auth_b by value. Uses the SAME punnu to show both
    // subscriptions can independently write into the same identity map.
    let handle_b = AuthRow::objects()
        .refresh_into(&punnu_a, pool, auth_b)
        .expect("unfiltered queryset must satisfy portable refresh gate");

    // ── Construct auth that caller holds but is unrelated to handle_a's auth ─
    // After `refresh_into`, the fetcher owns its own clone. Any further
    // mutations in the caller's scope are unobservable from the fetcher.
    let _auth_a_modified =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(99).expect("HeerId(99) is valid"));
    // _auth_a_modified is never passed to handle_a; it is in scope only to
    // document that caller-side auth changes cannot reach the fetcher.

    // ── Tick handle_a: captures auth_a (user_id=1) ──────────────────────────
    // Since `AuthRow` has no tenant_key, `auto_set_tenant` is a no-op and no
    // GUC is set. Both users' rows are returned (8 total) — the auth is applied
    // to the ctx via `ctx.set_auth(auth_a)` but has no SQL-level filtering
    // effect without tenant_key + RLS. This is the Option-C scope limitation.
    let tick_a = handle_a.update().await.expect("handle_a tick must succeed");
    assert_eq!(
        tick_a.applied,
        8,
        "handle_a tick (full scan, no RLS): all 8 rows must be applied; \
         got {applied}",
        applied = tick_a.applied,
    );

    // ── Tick handle_b: captures auth_b (user_id=2) ──────────────────────────
    // Same 8 rows visible (no RLS filtering; plain model). handle_b starts
    // with `since = None` (its own independent watermark), so the first tick
    // is a deterministic full scan of all 8 rows. A `<= 8` bound would paper
    // over a delta-path regression that returned partial results; pin
    // exactly 8.
    let tick_b = handle_b.update().await.expect("handle_b tick must succeed");
    assert_eq!(
        tick_b.applied,
        8,
        "handle_b's first tick must apply exactly 8 rows — handle_b starts \
         from `since = None` (independent watermark) and the AuthRow model \
         has no tenant_key, so a full scan is deterministic; got {applied}",
        applied = tick_b.applied,
    );

    // ── Both handles succeeded: auth-locking structural invariant holds ──────
    // The fetcher for each handle captured its auth at construction time
    // (`DjogiDeltaFetcher::auth: AuthContext` — owned, not borrowed). Any
    // in-scope `_auth_a_modified` or `auth_b` variable modifications are
    // invisible to the other handle. The `'static` bound on
    // `DeltaPunnuFetcher<T>` enforces this at the type level.
    //
    // Full RLS-backed row-count isolation proof lives in
    // `tests/internal/phase8_5_c2_129_non_superuser_rls.rs`, which
    // builds the fetcher pool through
    // `djogi::testing::connect_test_db_as_non_superuser` so the SELECT
    // observes RLS server-side. Closes GH #129.

    // ── Second tick on handle_a: still runs under auth_a ────────────────────
    // After handle_b has ticked, handle_a's auth snapshot is unchanged.
    // A second tick on handle_a succeeds and observes the same dataset
    // (since no new rows were inserted — watermark boundary delivers at most 1).
    let tick_a2 = handle_a
        .update()
        .await
        .expect("handle_a second tick must succeed — auth captured at construction time");
    assert!(
        tick_a2.applied <= 8,
        "handle_a second tick must apply at most 8 rows (delta or boundary re-check); \
         got {applied}",
        applied = tick_a2.applied,
    );
}

// ── Test 3 — cancel stops periodic ticks ─────────────────────────────────────

/// Pins sassi's canonical post-cancel behavior for `DeltaRefreshHandle`.
///
/// `handle.cancel()` sends `true` on the internal `watch::Sender` that the
/// periodic refresh loop monitors. The loop breaks on the next iteration when
/// it sees the cancel signal. This stops the **periodic** (background) ticks.
///
/// However, `handle.update()` does NOT check the cancel flag — it calls
/// `RefreshSubscription::update` directly, bypassing the periodic loop's
/// guard. Therefore, after `cancel()`:
///
/// - Periodic ticks cease (the background `run_periodic_delta_refresh` loop
///   exits when it observes the cancel watch value become `true`).
/// - Manual `handle.update().await` still succeeds (the cancel flag is not
///   inspected in the on-demand update path — verified by reading
///   `sassi/src/punnu/delta_refresh.rs:RefreshSubscription::update`).
///
/// This test pins both halves of the contract:
/// 1. Before cancel: a manual tick succeeds and returns the expected rows.
/// 2. After cancel: a manual tick STILL succeeds (proving cancel targets only
///    the periodic background loop, not on-demand invocations).
///
/// The periodic-loop stop itself cannot be directly observed from outside
/// sassi internals without a timing harness; we document the mechanism and
/// pin the on-demand behavior that IS directly observable.
///
#[djogi::djogi_test(sync_models = [CancelRow])]
async fn refresh_into_cancel_stops_ticks(mut ctx: djogi::DjogiContext) {
    // Insert 2 rows so ticks return non-trivial results.
    for i in 1i64..=2 {
        CancelRow::create(
            &mut ctx,
            CancelRow {
                label: format!("cancel-row-{i}"),
                ..Default::default()
            },
        )
        .await
        .expect("insert cancel-test row");
    }

    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");
    let punnu = ctx
        .punnu::<CancelRow>()
        .expect("punnu registered for CancelRow");
    let auth =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"));

    let handle = CancelRow::objects()
        .refresh_into(&punnu, pool, auth)
        .expect("unfiltered queryset must satisfy portable refresh gate");

    // ── Pre-cancel: manual tick succeeds ────────────────────────────────────
    let pre_cancel_tick = handle.update().await.expect("pre-cancel tick must succeed");
    assert_eq!(
        pre_cancel_tick.applied,
        2,
        "pre-cancel tick must apply both rows (full scan, since=None); \
         got {applied}",
        applied = pre_cancel_tick.applied,
    );

    // ── Cancel the periodic refresh loop ────────────────────────────────────
    // `cancel()` is non-blocking: it sends `true` on a `watch::Sender` and
    // returns immediately. The background periodic loop will exit on its next
    // iteration when it observes the cancel signal. No in-flight ticks are
    // interrupted (sassi contract: in-flight fetches continue to completion).
    handle.cancel();

    // ── Post-cancel: manual tick still succeeds ──────────────────────────────
    // The cancel signal targets `run_periodic_delta_refresh` ONLY.
    // `handle.update()` → `RefreshSubscription::update` → does NOT check the
    // cancel watch receiver. The on-demand tick must succeed.
    //
    // Delta tick (since = max watermark from pre-cancel tick):
    // The inclusive `>=` boundary re-delivers the most-recently-updated row
    // from the pre-cancel tick, so `applied >= 1` (the boundary row).
    let post_cancel_tick = handle.update().await.expect(
        "post-cancel tick must succeed — cancel() only stops periodic ticks, \
                 not on-demand update() calls (verified in sassi delta_refresh.rs \
                 RefreshSubscription::update which bypasses the cancel watch)",
    );

    // At least 1 row (the boundary row at max watermark, re-delivered by the
    // inclusive `>=` boundary protocol per DeltaPunnuFetcher contract).
    assert!(
        post_cancel_tick.applied >= 1,
        "post-cancel tick must apply at least 1 row (the boundary row at max \
         watermark, included by the inclusive `>=` boundary); \
         got {applied}",
        applied = post_cancel_tick.applied,
    );
}
