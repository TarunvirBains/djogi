//! Phase 8δ T8.9 integration tests: `DeltaRefreshHandle<T>` lifetime audit —
//! owned substrate captures across thread boundary.
//!
//! # What this file pins
//!
//! 1. **`refresh_handle_survives_tokio_spawn`** — proves the handle is `Send`:
//!    constructs a handle in the test's main async scope, moves it into
//!    `tokio::spawn(async move { ... })`, calls `update().await` from the
//!    spawned task, and asserts the tick succeeded. If the handle were not
//!    `Send` this test would not compile.
//!
//! 2. **`refresh_handle_survives_pool_drop_in_main`** — pins pool clone
//!    independence. `DjogiPool` is `Arc`-internal; the fetcher captures its
//!    own clone at `refresh_into` construction time. Dropping the original
//!    pool handle in the caller's scope decrements the refcount but the
//!    fetcher's clone keeps the pool alive. `handle.update().await` must
//!    succeed after `drop(original_pool)`.
//!
//! 3. **`refresh_handle_send_sync_compile_check`** — compile-time regression
//!    guard for `Send + Sync`. A zero-body `fn requires_send_sync<T: Send + Sync>()`
//!    witness prevents any future refactor from silently breaking the bounds
//!    without a build error.
//!
//! # Why these three tests belong together
//!
//! Together they form the "lifetime audit" for the owned-substrate invariant
//! landed in T8.3: a `DjogiDeltaFetcher<T>` holds no borrowed references —
//! only owned, `'static` values — so `DeltaRefreshHandle<T>` can freely cross
//! thread and lifetime boundaries. If T8.3's invariant were accidentally broken
//! (e.g. a borrowed `DjogiContext` added to the fetcher), Test 1 and Test 3
//! would fail to compile, surfacing the regression before it reaches production.
//!
//! # Spec anchor
//!
//! `docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md`
//! §3 commit T8.9.
//!
//! # Fixture strategy
//!
//! A fresh model type (`LifetimeRow`) with its own table
//! (`phase8_t8_9_lifetime_rows`) is used throughout. A separate type avoids
//! `inventory` descriptor dedup conflicts with `FetcherTickRow` /
//! `KnobRow` / etc. from other test binaries that register the same
//! singleton `ModelDescriptor` under their respective type names.
//!
//! Each test provisions its own table inline via `ctx.raw_execute`. The
//! `#[djogi_test]` macro installs the HeeRanjID schema, seeds node 1, and sets
//! `heer.node_id = '1'` before the test body runs.

use djogi::prelude::*;

// ── Fixture model ─────────────────────────────────────────────────────────────
//
// `LifetimeRow` lives in its own table so it never collides with other test
// models in the descriptor inventory. `#[derive(Clone)]` is required by
// `Cacheable + Punnu<T>`. `pk = HeerId` pins the PK strategy to the standard
// ascending BIGINT so the test is independent of any future default-PK change.

#[model(table = "phase8_t8_9_lifetime_rows", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct LifetimeRow {
    pub label: String,
}

async fn setup_lifetime_rows(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE IF NOT EXISTS phase8_t8_9_lifetime_rows (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            label       TEXT        NOT NULL
        )",
        &[],
    )
    .await
    .expect("create phase8_t8_9_lifetime_rows table");

    // Truncate between runs so rows from an earlier test do not bleed through.
    ctx.raw_execute("TRUNCATE phase8_t8_9_lifetime_rows", &[])
        .await
        .expect("truncate phase8_t8_9_lifetime_rows");
}

// ── Test 1 — handle is Send: moves into tokio::spawn ─────────────────────────

/// Construct a `DeltaRefreshHandle<LifetimeRow>` in the test's main scope,
/// move it into `tokio::spawn(async move { ... })`, and call `update().await`
/// from the spawned task.
///
/// This test is a compile-time + runtime pin for `DeltaRefreshHandle<T>: Send`.
/// If any future change to `DjogiDeltaFetcher<T>` (or to sassi's
/// `DeltaRefreshHandle<T>`) introduced a non-`Send` field, this test would
/// fail to compile — the `tokio::spawn(async move { handle.update().await })`
/// expansion requires `handle: Send` (the future must be `Send` for the
/// `tokio::spawn` bound).
///
/// A successful compile + green test jointly prove:
/// 1. The handle can cross a thread boundary (runtime proof via spawn).
/// 2. `update().await` works from a thread other than the one that constructed
///    the handle (exercising the "no thread-affinity" property of the owned
///    substrate).
#[djogi::djogi_test]
async fn refresh_handle_survives_tokio_spawn(mut ctx: djogi::DjogiContext) {
    setup_lifetime_rows(&mut ctx).await;

    // Insert one row so the tick returns a non-trivial result.
    ctx.raw_execute(
        "INSERT INTO phase8_t8_9_lifetime_rows (id, created_at, updated_at, label) \
         VALUES (generate_id(), now(), now(), 'spawn-test')",
        &[],
    )
    .await
    .expect("insert spawn-test row");

    let pool = ctx
        .pool()
        .expect("djogi_test context must have a pool")
        .clone();

    let punnu = ctx
        .punnu::<LifetimeRow>()
        .expect("punnu registered for LifetimeRow");

    let auth =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"));

    // Construct the handle in this scope.
    let handle = LifetimeRow::objects().refresh_into(&punnu, pool, auth);

    // Move the handle into a spawned task and call update() from there.
    // This compiles only if DeltaRefreshHandle<LifetimeRow>: Send.
    let join = tokio::spawn(async move {
        handle
            .update()
            .await
            .expect("update() must succeed from spawned task")
    });

    let result = join.await.expect("spawned task must not panic");

    assert_eq!(
        result.applied,
        1,
        "spawned update() must return 1 row (the 'spawn-test' row); \
         got {applied}",
        applied = result.applied
    );
}

// ── Test 2 — pool clone is independent of the caller's pool handle ────────────

/// Build a `DeltaRefreshHandle` from an explicitly-cloned pool, then drop the
/// original `DjogiPool` handle in the test's main scope. The handle must still
/// work because `refresh_into` captured an owned clone of the pool, not a
/// borrow.
///
/// `DjogiPool` uses `Arc`-internal reference counting: `drop(original_pool)`
/// decrements the refcount from 2 → 1 (the fetcher's clone keeps it at 1).
/// The pool's underlying connection manager is freed only when the last `Arc`
/// is dropped — which happens when the handle itself is dropped at the end of
/// this test, not before.
///
/// This test is a runtime pin for the "fetcher owns the pool" contract from
/// T8.3. A regression that stored a raw pointer or a borrowed reference to
/// the pool would cause a use-after-free or borrow-check error here.
#[djogi::djogi_test]
async fn refresh_handle_survives_pool_drop_in_main(mut ctx: djogi::DjogiContext) {
    setup_lifetime_rows(&mut ctx).await;

    // Insert one row so the tick is meaningful.
    ctx.raw_execute(
        "INSERT INTO phase8_t8_9_lifetime_rows (id, created_at, updated_at, label) \
         VALUES (generate_id(), now(), now(), 'pool-drop-test')",
        &[],
    )
    .await
    .expect("insert pool-drop-test row");

    // Clone the pool; the fetcher will capture this clone by value.
    let original_pool = ctx
        .pool()
        .expect("djogi_test context must have a pool")
        .clone();

    let punnu = ctx
        .punnu::<LifetimeRow>()
        .expect("punnu registered for LifetimeRow");

    let auth =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"));

    // Construct the handle. Internally `refresh_into` clones the pool into the
    // `DjogiDeltaFetcher` — the fetcher now holds its own Arc clone.
    let handle = LifetimeRow::objects().refresh_into(&punnu, original_pool.clone(), auth);

    // Drop the original pool handle. Arc refcount: fetcher's clone keeps it alive.
    drop(original_pool);

    // update() must succeed — the fetcher's pool clone is still live.
    let result = handle
        .update()
        .await
        .expect("update() must succeed after original pool handle is dropped");

    assert_eq!(
        result.applied,
        1,
        "update() after pool drop must return 1 row; got {applied}",
        applied = result.applied
    );
}

// ── Test 3 — compile-time Send + Sync regression guard ───────────────────────

/// Compile-time witness that `sassi::DeltaRefreshHandle<LifetimeRow>` is
/// `Send + Sync`.
///
/// `fn requires_send_sync<T: Send + Sync>()` is a zero-body function whose
/// only purpose is to enforce the bounds at the call site. If a future refactor
/// breaks `Send` or `Sync` on `DeltaRefreshHandle<T>` (or on any of its
/// transitive fields — including `DjogiDeltaFetcher<T>`, which is wrapped in
/// `Arc` inside sassi), this file will fail to compile rather than letting the
/// regression propagate silently.
///
/// Sassi already guarantees `DeltaRefreshHandle<T>: Send + Sync` via its own
/// internal `Arc<RefreshSubscription<T>>` wrapper plus `DeltaPunnuFetcher`'s
/// `Send + Sync + 'static` bound. This djogi-side witness pins the guarantee
/// at the djogi/sassi integration boundary — catching any version-skew between
/// the two crates before it reaches production.
///
/// The test body is intentionally empty beyond the compile-time assertion;
/// runtime verification is covered by Tests 1 and 2.
#[djogi::djogi_test]
async fn refresh_handle_send_sync_compile_check(mut ctx: djogi::DjogiContext) {
    setup_lifetime_rows(&mut ctx).await;

    // Compile-time witness: this call resolves only if
    // `sassi::DeltaRefreshHandle<LifetimeRow>: Send + Sync`.
    fn requires_send_sync<T: Send + Sync>() {}
    requires_send_sync::<sassi::DeltaRefreshHandle<LifetimeRow>>();

    // No runtime assertions needed — the compile-time check is sufficient.
    // The test is included in the harness so it appears in `cargo test` output
    // as an explicit regression guard, not just an implicit compile artifact.
}
