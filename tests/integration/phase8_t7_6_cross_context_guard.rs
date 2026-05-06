//! Phase 8δ T7.6 integration tests: `DjogiContext::use_punnu` cross-context guard.
//!
//! What this file pins:
//!
//! 1. `use_punnu_passes_when_same_context` — `ctx.use_punnu(&p)` where `p`
//!    was acquired from the same `ctx` returns an `Arc` that is
//!    `ptr_eq` to `p` (same allocation, no copy).
//!
//! 2. `use_punnu_panics_in_debug_on_mismatch` — passing a `Punnu<T>` from
//!    `ctx_a` to `ctx_b.use_punnu(...)` panics in debug builds with the
//!    "cross-context Punnu access" message.
//!
//! 3. `use_punnu_returns_empty_in_release` — same cross-context scenario but
//!    compiled in release mode (`#[cfg(not(debug_assertions))]`); instead of
//!    panicking, `use_punnu` returns a fresh empty `Punnu<T>`. The returned
//!    Arc is not `ptr_eq` to either source Punnu, and `len() == 0`.
//!
//! # Design anchor
//!
//! Per `feedback_decision_priorities.md`:
//! **scalability > production stability > idiomatic Rust > simple to use**.
//! The cfg-fork maps each build target to the dominant axis:
//! - Debug → panic (idiomatic Rust + scalability: surface misuse loudly).
//! - Release → tracing::error + empty fallback (production stability:
//!   fail closed without crashing a multi-tenant server).
//!
//! # Path X reframing
//!
//! The check uses `Arc::ptr_eq` (Arc-identity), NOT `TenantKey`. Under
//! cluster 8δ T7.4 Path X, `DjogiContext` IS the tenant boundary — each
//! context owns a fresh `Arc<Sassi>` with its own pool registry.
//!
//! # Fixture strategy
//!
//! `#[djogi_test]` gives us a pool-backed context. We use
//! `ctx.pool().clone()` + `DjogiContext::from_pool` to build two distinct
//! top-level contexts with independent `Sassi` registries. Tests 1 and 2
//! run in debug mode (the default `cargo test`). Test 3 is
//! `#[cfg(not(debug_assertions))]` and requires `--release`.
//!
//! # Spec anchor
//!
//! `docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md`
//! §3 commit T7.6.

use djogi::prelude::*;

// ---------------------------------------------------------------------------
// Fixture model — a unique type name avoids descriptor-inventory conflicts
// with T7.4/T7.5 fixtures that live in separate test binaries.
// ---------------------------------------------------------------------------

#[model(table = "phase8_t7_6_guard_rows", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Phase8T76GuardRow {
    pub label: String,
}

async fn setup_guard_row(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE IF NOT EXISTS phase8_t7_6_guard_rows (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            label       TEXT        NOT NULL
        )",
        &[],
    )
    .await
    .expect("create phase8_t7_6_guard_rows table");
}

// ---------------------------------------------------------------------------
// Test 1 — `use_punnu` returns the same Arc when the Punnu belongs to
// this context's Sassi.
// ---------------------------------------------------------------------------

#[djogi::djogi_test]
async fn use_punnu_passes_when_same_context(mut ctx: djogi::DjogiContext) {
    setup_guard_row(&mut ctx).await;

    let p = ctx
        .punnu::<Phase8T76GuardRow>()
        .expect("Phase8T76GuardRow must be registered via boot hook");

    let result = ctx.use_punnu(&p);

    assert!(
        std::sync::Arc::ptr_eq(&result, &p),
        "use_punnu must return an Arc that is ptr_eq to the input when \
         the Punnu was acquired from the same context — expected same allocation, \
         got different pointers",
    );
}

// ---------------------------------------------------------------------------
// Test 2 — `use_punnu` panics in debug builds when a Punnu from ctx_a is
// passed to ctx_b.use_punnu(). The two contexts have independent Sassi
// registries (T7.4 Path X), so ptr_eq returns false.
// ---------------------------------------------------------------------------

#[cfg(debug_assertions)]
#[djogi::djogi_test]
#[should_panic(expected = "cross-context Punnu access")]
async fn use_punnu_panics_in_debug_on_mismatch(mut ctx: djogi::DjogiContext) {
    setup_guard_row(&mut ctx).await;

    // Build two fresh top-level contexts from the same pool — each gets
    // its own Arc<Sassi> with an independent Punnu<Phase8T76GuardRow>.
    let pool = ctx
        .pool()
        .expect("djogi_test harness produces a pool-backed context")
        .clone();

    let ctx_a = djogi::DjogiContext::from_pool(pool.clone());
    let ctx_b = djogi::DjogiContext::from_pool(pool.clone());

    let p_a = ctx_a
        .punnu::<Phase8T76GuardRow>()
        .expect("ctx_a.punnu::<Phase8T76GuardRow>() must return Some");

    // Passing ctx_a's Punnu to ctx_b.use_punnu must panic in debug mode.
    let _ = ctx_b.use_punnu(&p_a);
}

// ---------------------------------------------------------------------------
// Test 3 — `use_punnu` returns an empty Punnu (not ptr_eq to either source)
// in release builds when the Punnu belongs to a different context.
// Only compiled and run when debug_assertions are disabled (`--release`).
// ---------------------------------------------------------------------------

#[cfg(not(debug_assertions))]
#[djogi::djogi_test]
async fn use_punnu_returns_empty_in_release(mut ctx: djogi::DjogiContext) {
    setup_guard_row(&mut ctx).await;

    let pool = ctx
        .pool()
        .expect("djogi_test harness produces a pool-backed context")
        .clone();

    let ctx_a = djogi::DjogiContext::from_pool(pool.clone());
    let ctx_b = djogi::DjogiContext::from_pool(pool.clone());

    let p_a = ctx_a
        .punnu::<Phase8T76GuardRow>()
        .expect("ctx_a.punnu::<Phase8T76GuardRow>() must return Some");
    let p_b = ctx_b
        .punnu::<Phase8T76GuardRow>()
        .expect("ctx_b.punnu::<Phase8T76GuardRow>() must return Some");

    // In release mode this must NOT panic; it returns a fresh empty Punnu.
    let result = ctx_b.use_punnu(&p_a);

    assert!(
        !std::sync::Arc::ptr_eq(&result, &p_a),
        "release fallback must be a fresh allocation — not ptr_eq to the \
         mismatched Punnu from ctx_a",
    );
    assert!(
        !std::sync::Arc::ptr_eq(&result, &p_b),
        "release fallback must be a fresh allocation — not ptr_eq to \
         ctx_b's own registered Punnu",
    );
    assert_eq!(
        result.len(),
        0,
        "release fallback Punnu must be empty (len == 0) — reads must \
         return None and writes must not propagate",
    );
}
