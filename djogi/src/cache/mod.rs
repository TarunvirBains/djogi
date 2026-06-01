//! Re-exports sassi cache primitives so adopters can `use djogi::cache::*;`
//! without an explicit sassi dep in their Cargo.toml.
//! Spec: `docs/spec/maahi/caching.md` ("Why sassi").
//! # Why this module exists
//! Wires sassi's typed in-memory pool (`Punnu<T>`) into djogi
//! as the canonical L1 cache. Adopter code that constructs a `Punnu`,
//! observes `PunnuEvent`s, attaches an L2 [`CacheBackend`], or composes
//! `MemQ` scopes only ever needs `djogi` in their `Cargo.toml`
//! reaching for sassi types directly through `djogi::cache::*`. The
//! framework absorbs the dependency, and the macro layer (T7.2) emits
//! `Cacheable` impls without forcing adopters to learn the sassi
//! crate name.
//! # What this module does NOT export
//! The `sassi-macros::Cacheable` derive is intentionally NOT re-exported.
//! Djogi has its own `#[derive(Model)]` (via `#[model]`) which auto-emits
//! the `Cacheable` impl through `sassi-codegen` (T7.2 in the same
//! cluster). Re-exporting `sassi::Cacheable` (the derive) would create
//! two ways to reach the same trait impl and tempt adopters into mixing
//! the two surfaces. The trait alone is re-exported here; the derive
//! flows through `#[model]` only.
//! # Macro routing
//! Per `feedback_macro_path_routing.md`, macro-emitted code never spells
//! `::sassi::*` paths directly — `crate::types` re-exports `Cacheable`,
//! `DeltaSyncCacheable`, `MonotonicWatermark`, and `BasicPredicate` so
//! T7.2's emitted impls write `::djogi::types::Cacheable for …` instead.
//! This module is the adopter-facing surface; `crate::types` is the
//! macro-emission target. Both paths resolve to the same sassi types.

// `Cacheable` (the TRAIT) is imported from `sassi::cacheable::Cacheable`,
// not `sassi::Cacheable` — the latter glob-resolves to BOTH the trait
// and `sassi_macros::Cacheable` (the derive macro), because sassi
// re-exports both under the same identifier (`sassi/src/lib.rs:78` for
// the trait, `sassi/src/lib.rs:97` for the derive). Routing through
// the module path `sassi::cacheable` yields just the trait — the
// derive lives in `sassi_macros` and never enters this re-export tree.
// This honours the "do not re-export the derive" pledge in the module
// doc above.
pub use sassi::cacheable::Cacheable;
pub use sassi::{
    BackendInvalidation, BackendInvalidationStream, BackendKeyspace, BasicPredicate, CacheBackend,
    DeltaApplyStats, DeltaPunnuFetcher, DeltaQuery, DeltaRefreshHandle, DeltaResult,
    DeltaSyncCacheable, EventReason, FetchError, InsertError, InvalidationReason, MemQ,
    MonotonicWatermark, OnConflict, Punnu, PunnuBuilder, PunnuConfig, PunnuEvent, PunnuMetrics,
    PunnuScope, RefreshHandle, Sassi, TenantKey, UpdateResult,
};

// 4 — boot-time `Sassi` registration via inventory.
// `SassiBootHook` is the newtype `#[derive(Model)]`-emitted
// `inventory::submit!` blocks deposit. `DjogiContext::from_pool` (and
// `from_connection`) walk `inventory::iter::<SassiBootHook>` once and
// freeze the result into `Arc<Sassi>`.
// Re-exported at the crate root (`djogi::SassiBootHook`) so macro-emitted
// code can spell `::djogi::SassiBootHook` per
// `feedback_macro_path_routing.md`.
// GH #125 — `SassiBootHook` is link-time machinery, not adopter-facing
// API. Hide both the module path and the re-export so `cargo doc` does
// not surface the inventory wiring through `djogi::cache::*`.
#[doc(hidden)]
pub mod boot;
#[doc(hidden)]
pub use boot::SassiBootHook;

/// SQL column name that carries the delta-sync watermark for a
/// `#[model]`-annotated struct.
/// # What
/// `DjogiDeltaSyncMeta` is auto-emitted by `#[derive(Model)]` alongside
/// `Cacheable` and `DeltaSyncCacheable`. It surfaces the column name
/// corresponding to `DeltaSyncCacheable::watermark` so the
/// `DjogiDeltaFetcher` (5) can emit
/// `WHERE <col> >= $since` SQL on every tick without string literals in
/// framework internals.
/// # Relationship to `DeltaSyncCacheable`
/// `DeltaSyncCacheable::watermark` returns the Rust value;
/// `DjogiDeltaSyncMeta::WATERMARK_COLUMN` names the Postgres column that
/// stores that value. The two are always consistent because the macro
/// emits both from the same `watermark_field` attribute (or its
/// `updated_at` default).
/// # Macro routing
/// Per `feedback_macro_path_routing.md`, macro-emitted `impl` blocks route
/// through `::djogi::cache::DjogiDeltaSyncMeta`, never `::sassi::*`.
pub trait DjogiDeltaSyncMeta {
    /// The Postgres column name used as the delta-sync watermark.
    /// Interpolated directly into
    /// `SELECT … FROM t WHERE {col} >= $n ORDER BY {col}` SQL text
    /// (where `{col}` is this constant's value). The name must be a valid
    /// Postgres identifier; the `#[model]` field-name validator enforces this
    /// at macro time.
    const WATERMARK_COLUMN: &'static str;
}
