//! Re-exports sassi cache primitives so adopters can `use djogi::cache::*;`
//! without an explicit sassi dep in their Cargo.toml.
//!
//! Spec: `docs/spec/maahi/caching.md` ("Why sassi") + Phase 8 plan §T7
//! (`docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md`
//! commit T7.1).
//!
//! # Why this module exists
//!
//! Cluster 8δ wires sassi's typed in-memory pool (`Punnu<T>`) into djogi
//! as the canonical L1 cache. Adopter code that constructs a `Punnu`,
//! observes `PunnuEvent`s, attaches an L2 [`CacheBackend`], or composes
//! `MemQ` scopes only ever needs `djogi` in their `Cargo.toml` —
//! reaching for sassi types directly through `djogi::cache::*`. The
//! framework absorbs the dependency, and the macro layer (T7.2) emits
//! `Cacheable` impls without forcing adopters to learn the sassi
//! crate name.
//!
//! # What this module does NOT export
//!
//! The `sassi-macros::Cacheable` derive is intentionally NOT re-exported.
//! Djogi has its own `#[derive(Model)]` (via `#[model]`) which auto-emits
//! the `Cacheable` impl through `sassi-codegen` (T7.2 in the same
//! cluster). Re-exporting `sassi::Cacheable` (the derive) would create
//! two ways to reach the same trait impl and tempt adopters into mixing
//! the two surfaces. The trait alone is re-exported here; the derive
//! flows through `#[model]` only.
//!
//! # Macro routing
//!
//! Per `feedback_macro_path_routing.md`, macro-emitted code never spells
//! `::sassi::*` paths directly — `crate::types` re-exports `Cacheable`,
//! `DeltaSyncCacheable`, `MonotonicWatermark`, and `BasicPredicate` so
//! T7.2's emitted impls write `::djogi::types::Cacheable for …` instead.
//! This module is the adopter-facing surface; `crate::types` is the
//! macro-emission target. Both paths resolve to the same sassi types.

pub use sassi::{
    BackendInvalidation, BackendInvalidationStream, BackendKeyspace, BasicPredicate, CacheBackend,
    Cacheable, DeltaApplyStats, DeltaPunnuFetcher, DeltaQuery, DeltaRefreshHandle, DeltaResult,
    DeltaSyncCacheable, EventReason, FetchError, InsertError, InvalidationReason, MemQ,
    MonotonicWatermark, OnConflict, Punnu, PunnuBuilder, PunnuConfig, PunnuEvent, PunnuMetrics,
    PunnuScope, RefreshHandle, Sassi, TenantKey, UpdateResult,
};
