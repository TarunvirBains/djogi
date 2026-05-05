// Cluster 8δ T7.1 — `djogi::cache` re-export module compile-pass.
//
// Verifies that an adopter can `use djogi::cache::*;` and reach every
// sassi cache primitive without listing `sassi` in their own
// `Cargo.toml`. The trybuild harness builds this fixture against the
// public `djogi` crate exactly as an external adopter would, so any
// hidden transitive sassi dep on the test path would still produce a
// compile failure if `djogi::cache` failed to re-export the symbol.
//
// The companion runtime tests in `djogi/tests/cache_module.rs`
// exercise `Punnu::builder().build()` end-to-end; this fixture covers
// the type-level reachability of every spec-named symbol so a future
// removal-by-typo from `cache.rs` is caught at fixture-build time.
//
// Per `feedback_trybuild_fixtures.md`, every trybuild fixture must
// have `fn main` so the stored binary can link.
//
// Spec anchor:
//   docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md
//   §3 commit T7.1 — "Test names + assertions" bullet.

#![allow(unused_imports)]

// The `use` block is the load-bearing surface check: every symbol the
// spec mandates must resolve here, or rustc fails name-resolution
// before any function body is evaluated. Traits, structs, enums, and
// type aliases all flow through the same import surface — if any of
// them is missing from `djogi::cache`, this `use` fails with E0432.
//
// We import every name and rebind it under a `_unused_*` alias so a
// stray `unused_imports` lint cannot quietly suppress the surface
// check. The aliases are never referenced — their existence is the
// proof.
use djogi::cache::{
    BackendInvalidation as _UseBackendInvalidation,
    BackendInvalidationStream as _UseBackendInvalidationStream,
    BackendKeyspace as _UseBackendKeyspace, BasicPredicate as _UseBasicPredicate,
    CacheBackend as _UseCacheBackend, Cacheable as _UseCacheable,
    DeltaApplyStats as _UseDeltaApplyStats, DeltaPunnuFetcher as _UseDeltaPunnuFetcher,
    DeltaQuery as _UseDeltaQuery, DeltaRefreshHandle as _UseDeltaRefreshHandle,
    DeltaResult as _UseDeltaResult, DeltaSyncCacheable as _UseDeltaSyncCacheable,
    EventReason as _UseEventReason, FetchError as _UseFetchError, InsertError as _UseInsertError,
    InvalidationReason as _UseInvalidationReason, MemQ as _UseMemQ,
    MonotonicWatermark as _UseMonotonicWatermark, OnConflict as _UseOnConflict, Punnu as _UsePunnu,
    PunnuBuilder as _UsePunnuBuilder, PunnuConfig as _UsePunnuConfig,
    PunnuEvent as _UsePunnuEvent, PunnuMetrics as _UsePunnuMetrics, PunnuScope as _UsePunnuScope,
    RefreshHandle as _UseRefreshHandle, Sassi as _UseSassi, TenantKey as _UseTenantKey,
    UpdateResult as _UseUpdateResult,
};

// One additional usage check: the `Cacheable` trait is reachable as a
// bound on a generic function. This catches a regression where the
// trait might be re-exported under an alias that the trait-bound
// resolver cannot follow. (Concrete-type pins are exercised by the
// runtime tests in `djogi/tests/cache_module.rs`; trybuild scope is
// the type-level surface only.)
fn _bound_check<T>()
where
    T: djogi::cache::Cacheable,
{
}

fn main() {}
