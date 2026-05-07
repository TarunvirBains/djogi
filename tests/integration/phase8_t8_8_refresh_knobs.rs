// Phase 8δ T8.8 integration tests: `DeltaRefreshHandle` knobs — always-on
// LRU eviction warn and adopter reachability of `with_eviction_recovery` /
// `with_periodic_full_refresh`.
//
// # What this file pins
//
// 1. **`lru_warn_one_shot_per_subscription`** — creates a `Punnu` with
//    `lru_size: 1`, starts a `refresh_into` subscription, and inserts 2 rows
//    into the backing table. The first full-scan tick applies both rows to the
//    Punnu, causing an LRU eviction. The second tick's `fetch_delta` drains
//    the events receiver via `try_recv`, observes `LruEvict`, and emits a
//    one-shot `tracing::warn!` on `djogi::cache`. A third tick does NOT emit
//    the warn again (one-shot flag already set).
//
// 2. **`with_eviction_recovery_method_reachable`** — verifies that
//    `qs.refresh_into(...).with_eviction_recovery(true)` compiles and returns
//    `DeltaRefreshHandle<T>`. Sassi owns the runtime behavior; djogi only
//    verifies the method is reachable through its public surface.
//
// 3. **`with_periodic_full_refresh_method_reachable`** — verifies that
//    `qs.refresh_into(...).with_periodic_full_refresh(NonZeroUsize::new(10))`
//    compiles and returns `DeltaRefreshHandle<T>`.
//
// # Spec anchor
//
// `docs/spec/` §674 — DeltaRefreshHandle knobs. Production-stability
// motivation: silent LRU thrashing degrades cache hit rate without notifying
// adopters; the always-on warn surfaces undersized `lru_size` configs before
// they cause observable performance regressions (`feedback_decision_priorities.md`
// — production stability > simplicity).
//
// # Implementation choice
//
// Knob 1 (always-on LRU warn) uses Option B: `try_recv` per tick + `AtomicBool`
// one-shot flag inside `DjogiDeltaFetcher`. No separate spawned task is needed.
// Knobs 2 + 3 are sassi-native; djogi's `refresh_into` returns
// `DeltaRefreshHandle<T>` directly so the methods are reachable with no
// djogi-side wrappers.
//
// # Tracing capture
//
// Tests that assert on log output install a `tracing_test` global subscriber
// inline (via `tracing_test::internal`) rather than the `#[traced_test]`
// proc-macro attribute. The global buffer is append-only — each test snapshots
// the buffer length before its call and inspects only the new bytes appended
// since the snapshot. Tests are run with `--test-threads=1` to prevent
// concurrent writes from scrambling the buffer.

use djogi::prelude::*;
use std::num::NonZeroUsize;

// ── Fixture model ─────────────────────────────────────────────────────────────
//
// Separate table from T8.5/T8.6 fixtures for test isolation. `lru_size` is
// deliberately small in Test 1 to force evictions without inserting thousands
// of rows.

#[model(table = "phase8_t8_8_knob_row", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct KnobRow {
    pub label: String,
}

// ── Tracing capture helpers ───────────────────────────────────────────────────
//
// Mirror of the pattern used in phase8_t8_4_basic_predicate_extraction.rs and
// phase8_compose_auditable.rs. The global subscriber is installed exactly once
// via a `Once`; subsequent `init_log_capture()` calls just snapshot the
// current buffer length.

fn init_log_capture() -> usize {
    tracing_test::internal::INITIALIZED.call_once(|| {
        let buf = tracing_test::internal::global_buf();
        let mock_writer = tracing_test::internal::MockWriter::new(buf);
        let subscriber = tracing_test::internal::get_subscriber(mock_writer, "trace");
        tracing::dispatcher::set_global_default(subscriber).unwrap_or(());
    });
    tracing_test::internal::global_buf().lock().unwrap().len()
}

fn logs_since(since: usize) -> String {
    let buf = tracing_test::internal::global_buf().lock().unwrap();
    std::str::from_utf8(&buf[since..]).unwrap_or("").to_owned()
}

// ── Test 1 — always-on LRU eviction warn is one-shot per subscription ─────────

/// Verifies that:
///
/// 1. A `Punnu` with `lru_size: 1` emits an `LruEvict` event when a PREVIOUSLY
///    RESIDENT item is displaced by a new insert.
/// 2. The djogi fetcher's `try_recv` loop in `fetch_delta` observes the
///    `LruEvict` event on the NEXT tick after the eviction fires, and emits a
///    one-shot `tracing::warn!` on the `djogi::cache` target.
/// 3. The djogi warn fires exactly once (one-shot `AtomicBool` flag).
/// 4. Subsequent ticks do NOT re-emit the djogi warn.
///
/// # Sassi's own LRU warn
///
/// Sassi's `RefreshSubscription::note_lru_eviction` emits its own warn on
/// `sassi::punnu::delta_refresh` DURING Tick 2's `apply_delta`. The djogi warn
/// fires on a DIFFERENT target (`djogi::cache`) and DIFFERENT tick (Tick 3, via
/// `try_recv` at the start of `fetch_delta`). The two warns are complementary —
/// sassi warns at eviction time, djogi warns on the next tick when the
/// subscription re-evaluates. The test discriminates by searching for the
/// djogi-specific message substring ("undersized") which appears only in the
/// djogi warn, not in sassi's ("consider raising lru_size…").
///
/// # LruEvict event semantics
///
/// Sassi's `visible_lru_events` only includes victims that were already in the
/// Punnu BEFORE the `apply_delta` call. First-tick inserts into an empty Punnu
/// do NOT produce `LruEvict` events (no prior residents). A real `LruEvict`
/// event requires a second tick that displaces a resident item.
///
/// The test uses this sequence:
/// - Tick 1 (full scan): empty Punnu → inserts row A → no eviction.
/// - Tick 2 (watermark): Punnu at capacity with row A → inserts row B → evicts
///   row A → `LruEvict` event broadcast (sassi's own warn fires here too).
/// - Tick 3 (watermark): `try_recv` at START of `fetch_delta` drains the channel,
///   sees `LruEvict` from Tick 2 → djogi warn fires.
/// - Tick 4 + 5: another eviction cycle, but one-shot flag set → no second warn.
///
#[djogi::djogi_test(sync_models = [KnobRow])]
async fn lru_warn_one_shot_per_subscription(mut ctx: djogi::DjogiContext) {
    let since = init_log_capture();

    KnobRow::create(
        &mut ctx,
        KnobRow {
            label: "row-A".into(),
            ..Default::default()
        },
    )
    .await
    .expect("create row-A");

    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");

    // Build a standalone Punnu with lru_size=1. After Tick 1 loads row A, the
    // Punnu is at full capacity. Tick 2 inserts row B, which evicts row A and
    // fires an LruEvict event (sassi's own warn fires at this point). The djogi
    // warn fires on Tick 3 when try_recv drains the event channel.
    // Standalone Punnu (not ctx.punnu()) because the macro-registered Punnu has
    // lru_size=10_000 by default.
    let punnu: sassi::Punnu<KnobRow> = sassi::Punnu::builder()
        .config(sassi::PunnuConfig {
            lru_size: 1,
            ..Default::default()
        })
        .build();

    let auth =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"));

    let handle = KnobRow::objects().refresh_into(&punnu, pool.clone(), auth);

    // ── Tick 1 — full scan, inserts row A into empty Punnu ──────────────────
    //
    // Punnu is empty before this tick → no prior residents → no LruEvict event.
    let tick_1 = handle.update().await.expect("tick 1 must succeed");
    assert_eq!(
        tick_1.applied,
        1,
        "tick 1 must apply exactly row-A; got {applied}",
        applied = tick_1.applied
    );

    // No djogi LRU warn yet (djogi-specific substring "undersized" absent).
    let logs_after_tick_1 = logs_since(since);
    assert!(
        !logs_after_tick_1.contains("undersized"),
        "no djogi LRU warn expected after tick 1 (Punnu was empty, no resident evicted); \
         logs so far: {logs_after_tick_1:?}"
    );

    // Insert row B after Tick 1 so it passes the watermark filter.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    KnobRow::create(
        &mut ctx,
        KnobRow {
            label: "row-B".into(),
            ..Default::default()
        },
    )
    .await
    .expect("create row-B");

    // ── Tick 2 — watermark filter fetches row B, evicts row A ───────────────
    //
    // Sassi's own warn fires during apply_delta (on sassi::punnu::delta_refresh
    // target). The djogi try_recv check runs BEFORE the SQL at the start of
    // fetch_delta — it sees no events yet (Tick 1 produced none). The djogi
    // warn fires on Tick 3.
    let tick_2 = handle.update().await.expect("tick 2 must succeed");
    assert!(
        tick_2.applied >= 1,
        "tick 2 must apply at least row-B; got {applied}",
        applied = tick_2.applied
    );

    // Djogi LRU warn still absent after tick 2 (djogi's try_recv runs at the
    // START of a tick, so the Tick 2 eviction is only seen on Tick 3).
    let logs_after_tick_2 = logs_since(since);
    assert!(
        !logs_after_tick_2.contains("undersized"),
        "djogi LRU warn must NOT fire yet after tick 2 (try_recv at start of \
         tick 2 saw no events; tick 2's eviction is queued for tick 3); \
         logs so far: {logs_after_tick_2:?}"
    );

    // ── Tick 3 — try_recv sees LruEvict from Tick 2 → djogi warn fires ──────
    //
    // try_recv at the START of tick 3's fetch_delta drains the broadcast channel,
    // finds the LruEvict event from Tick 2's apply_delta, sets the AtomicBool
    // one-shot flag, and emits a warn on the `djogi::cache` tracing target.
    let tick_3 = handle.update().await.expect("tick 3 must succeed");
    let _ = tick_3;

    let logs_after_tick_3 = logs_since(since);
    assert!(
        logs_after_tick_3.contains("djogi::cache"),
        "djogi LRU warn must be targeted at the djogi::cache tracing target \
         (sassi's own LRU warn fires at sassi::punnu::delta_refresh — different \
          target); logs so far: {logs_after_tick_3:?}"
    );
    assert!(
        logs_after_tick_3.contains("undersized"),
        "djogi LRU eviction warn must contain the unique 'undersized' marker \
         (sassi's warn message is 'consider raising lru_size'); logs so far: \
         {logs_after_tick_3:?}"
    );

    // Count occurrences of the djogi LRU warn specifically by the
    // message-body marker `undersized`. We use this rather than the
    // `djogi::cache` target alone for forward-compatibility: today the
    // `djogi::cache` target is unique to the LRU warn (the filter-pushdown
    // warn at refresh.rs:128 is gated on a non-trivial filter, and T8.4's
    // reducer returns `Some(BasicPredicate::True)` for unfiltered querysets
    // — refresh_into strips that to `None` so the filter-pushdown warn
    // doesn't fire per-tick for the unfiltered case this test runs). But
    // if a future change re-introduces another `djogi::cache`-targeted warn
    // (e.g. GH #127 lands a real filter-pushdown emitter that fires on
    // partial-pushdown failure), counting by target would silently double-
    // count. The `undersized` token is unique to the LRU warn within djogi's
    // codebase; sassi's LRU warn at `sassi::punnu::delta_refresh` says
    // "consider raising lru_size" (distinct token). The structural
    // `djogi::cache` target match above pins the structural contract; this
    // count pins the quantitative one-shot contract.
    let djogi_warn_count = logs_after_tick_3.matches("undersized").count();
    assert_eq!(
        djogi_warn_count, 1,
        "djogi LRU eviction warn must fire exactly once (one-shot AtomicBool); \
         found {djogi_warn_count} 'undersized' occurrences in logs: {logs_after_tick_3:?}"
    );

    // ── Tick 4 + 5 — one-shot flag already set → no second djogi warn ────────
    //
    // Insert row C after Tick 3 so Tick 4 has data and produces another
    // eviction. The one-shot flag must prevent a second djogi warn.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    KnobRow::create(
        &mut ctx,
        KnobRow {
            label: "row-C".into(),
            ..Default::default()
        },
    )
    .await
    .expect("create row-C");

    // Tick 4: another eviction (row B evicted by row C). Sassi's warn may fire
    // again (sassi has its own flag per subscription, so it should NOT repeat
    // either, but we don't assert on sassi's behavior here). The djogi flag
    // is already set.
    let tick_4 = handle.update().await.expect("tick 4 must succeed");
    let _ = tick_4;

    // Tick 5: try_recv would see LruEvict from Tick 4, but one-shot flag set.
    let tick_5 = handle.update().await.expect("tick 5 must succeed");
    let _ = tick_5;

    let logs_after_tick_5 = logs_since(since);
    let djogi_warn_count_final = logs_after_tick_5.matches("undersized").count();
    assert_eq!(
        djogi_warn_count_final,
        djogi_warn_count,
        "djogi LRU warn must NOT repeat after tick 5 (one-shot flag prevents \
         double-warn); expected {djogi_warn_count} total, got \
         {djogi_warn_count_final}; \
         new logs since tick 3: {:?}",
        &logs_after_tick_5[logs_after_tick_3.len()..],
    );
}

// ── Test 2 — with_eviction_recovery is adopter-reachable ─────────────────────

/// Verifies that `qs.refresh_into(...).with_eviction_recovery(true)` compiles
/// and returns `sassi::DeltaRefreshHandle<KnobRow>`.
///
/// Sassi owns the runtime behavior of eviction recovery. Djogi's contract is
/// only that the method is reachable through the `refresh_into` return type
/// without a djogi-side wrapper. This test is a compile-pass verification —
/// it calls the method and asserts the returned type is usable as a handle.
///
#[djogi::djogi_test(sync_models = [KnobRow])]
async fn with_eviction_recovery_method_reachable(mut ctx: djogi::DjogiContext) {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");

    let punnu = ctx
        .punnu::<KnobRow>()
        .expect("punnu registered for KnobRow");

    let auth =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"));

    // The `with_eviction_recovery` call must compile and return the handle.
    let handle = KnobRow::objects()
        .refresh_into(&punnu, pool, auth)
        .with_eviction_recovery(true);

    // Verify the handle is usable: cancel it so sassi cleans up the
    // subscription (no-op if already stopped; just proves the type is `DeltaRefreshHandle<T>`).
    handle.cancel();
}

// ── Test 3 — with_periodic_full_refresh is adopter-reachable ─────────────────

/// Verifies that
/// `qs.refresh_into(...).with_periodic_full_refresh(NonZeroUsize::new(10))`
/// compiles and returns `sassi::DeltaRefreshHandle<KnobRow>`.
///
/// Sassi owns the runtime behavior of periodic full refreshes. Djogi's
/// contract is that the method is reachable through the `refresh_into` return
/// type without a djogi-side wrapper.
///
#[djogi::djogi_test(sync_models = [KnobRow])]
async fn with_periodic_full_refresh_method_reachable(mut ctx: djogi::DjogiContext) {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");

    let punnu = ctx
        .punnu::<KnobRow>()
        .expect("punnu registered for KnobRow");

    let auth =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"));

    // The `with_periodic_full_refresh` call must compile and return the handle.
    let handle = KnobRow::objects()
        .refresh_into(&punnu, pool, auth)
        .with_periodic_full_refresh(NonZeroUsize::new(10));

    // Verify the progress accessor works (sassi native method, not djogi-wrapped).
    let progress = handle.periodic_full_refresh_progress();
    assert!(
        progress.is_some(),
        "periodic_full_refresh_progress must return Some after with_periodic_full_refresh(Some(10))"
    );

    handle.cancel();
}
