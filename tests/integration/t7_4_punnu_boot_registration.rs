// T7.4 integration tests: `DjogiContext::punnu<T>()` boot-time
// inventory registration.
//
// What this file pins:
//
// 1. `ctx.punnu::<MyModel>()` returns `Some` for any `#[model]` struct
//    that derives a `Cacheable` impl (every model with a PK). The boot
//    hook emitted by `#[derive(Model)]` runs before the first
//    `DjogiContext::from_pool` call and registers the pool.
//
// 2. Two calls to `ctx.punnu::<T>()` on the same context return the same
//    `Arc<Punnu<T>>` — `Arc::ptr_eq` returns `true`, proving that the
//    registry hands out the same handle on every access.
//
// 3. Two distinct top-level `DjogiContext` instances (from the same pool)
//    each hold independent `Sassi` instances. Inserting into one context's
//    `Punnu<T>` does NOT affect the other context's `Punnu<T>`. This is
//    the "DjogiContext IS the tenant boundary" contract from cluster 8δ T7.4.
//
//    NOTE: Test 3 uses `ctx.share_pool()` to obtain the underlying pool for
//    constructing two sibling contexts via `DjogiContext::from_pool`. This
//    is a genuine typed-surface gap — there is no public non-bypass API to
//    `DjogiContext::share_pool() -> DjogiPool` (or `DjogiContext::sibling()`)
//
// # Spec anchor
//
// `docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md`
// §3 commit T7.4.
//
// # Fixture strategy
//
// Tables are provisioned via `#[djogi_test(sync_models = [PunnuBootRow])]`
// which routes through the same migration engine that production uses.
// The `#[djogi_test]` macro already installs HeeRanjID schema, seeds
// node 1, and sets `heer.node_id = '1'` before the test body runs.
//
// # Why these tests live in `tests/integration/`
//
// Per the workspace convention (every other `phase{N}_*` integration
// test sits here, registered through `djogi/Cargo.toml`'s `[[test]]`
// blocks). The `punnu()` surface is reachable through the public
// `djogi` crate API, exactly as adopters consume it.

use djogi::prelude::*;

// ---------------------------------------------------------------------------
// Fixture model — a tiny table whose rows we'll use for the boot-hook tests.
// `#[derive(Clone)]` is required by `Cacheable` + `Punnu<T>`.
// ---------------------------------------------------------------------------

#[model(table = "phase8_t7_4_punnu_boot_rows", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct PunnuBootRow {
    pub label: String,
}

// ---------------------------------------------------------------------------
// Test 1 — `ctx.punnu::<PunnuBootRow>()` returns `Some` for a
// `#[model]`-derived struct. The boot hook registered the pool at
// `DjogiContext::from_pool` time.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [PunnuBootRow])]
async fn punnu_registered_for_default_model(mut ctx: djogi::DjogiContext) {
    let pool = ctx.punnu::<PunnuBootRow>();

    assert!(
        pool.is_some(),
        "ctx.punnu::<PunnuBootRow>() must return Some — the #[model] macro emits \
         an inventory::submit! SassiBootHook that DjogiContext::from_pool walks; \
         None here means the boot hook was not emitted or the inventory was not \
         walked at context construction time",
    );
}

// ---------------------------------------------------------------------------
// Test 2 — Two calls to `ctx.punnu::<PunnuBootRow>()` return the same
// Arc, proving the pool handle is stable across calls on the same context.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [PunnuBootRow])]
async fn punnu_returns_same_pool_across_calls(mut ctx: djogi::DjogiContext) {
    let a = ctx
        .punnu::<PunnuBootRow>()
        .expect("first punnu call must return Some");
    let b = ctx
        .punnu::<PunnuBootRow>()
        .expect("second punnu call must return Some");

    assert!(
        std::sync::Arc::ptr_eq(&a, &b),
        "two calls to ctx.punnu::<T>() on the same context must return the same Arc — \
         the Punnu is registered once at boot time and the same handle is returned \
         on every access. Arc::ptr_eq failure means a new Punnu is allocated per \
         call, which would silently split the cache into N disjoint pools",
    );
}

// ---------------------------------------------------------------------------
// Test 3 — Cross-tenant contract: distinct top-level DjogiContext instances
// hold independent Sassi registries. Inserting into one context's Punnu
// does NOT populate the other context's Punnu.
//
// This is the load-bearing contract for "DjogiContext IS the tenant
// boundary" (cluster 8δ T7.4). Cluster 8δ T7.6 will add a runtime guard
// that enforces single-context use for tenant-keyed models; T7.4 just
// establishes and pins the boundary.
//
// underlying pool for constructing two sibling contexts via
// (or a `DjogiContext::sibling()` factory) so tests that verify the
// multi-context tenant-boundary contract do not need the bypass.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [PunnuBootRow])]
async fn cross_tenant_requires_separate_context(mut ctx: djogi::DjogiContext) {
    // Extract the underlying pool so we can build two fresh top-level contexts.
    let pool = ctx
        .share_pool()
        .expect("djogi_test harness produces a pool-backed context");

    let ctx_a = djogi::DjogiContext::from_pool(pool.clone());
    let ctx_b = djogi::DjogiContext::from_pool(pool.clone());

    let pool_a = ctx_a
        .punnu::<PunnuBootRow>()
        .expect("ctx_a.punnu::<PunnuBootRow>() must return Some");
    let pool_b = ctx_b
        .punnu::<PunnuBootRow>()
        .expect("ctx_b.punnu::<PunnuBootRow>() must return Some");

    // The two Arcs must point at DIFFERENT Punnu instances — each
    // DjogiContext builds its own Sassi at construction time.
    assert!(
        !std::sync::Arc::ptr_eq(&pool_a, &pool_b),
        "pool_a and pool_b must be distinct Arc<Punnu<T>> instances — different \
         DjogiContext instances each own independent Sassi registries; if this \
         assertion fails it means the framework is sharing a single Sassi across \
         unrelated contexts, which would violate the tenant-boundary contract",
    );

    // Insert a value into pool_a. pool_b must remain empty — they are
    // independent registries with no shared state.
    let row = PunnuBootRow {
        label: "cross_tenant_test".into(),
        ..Default::default()
    };
    pool_a
        .insert(row)
        .await
        .expect("Punnu::insert into pool_a must succeed");

    assert_eq!(pool_a.len(), 1, "pool_a must contain the inserted row",);
    assert_eq!(
        pool_b.len(),
        0,
        "pool_b must remain empty after inserting into pool_a — the two Punnu \
         instances are independent; a non-zero count here means sassi's Punnu \
         internals are accidentally sharing state across Arc clones, or the \
         framework is incorrectly sharing the same Arc<Punnu<T>> between contexts",
    );
}
