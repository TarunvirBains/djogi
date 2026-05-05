//! Boot-time `Sassi` registration via the `inventory` crate.
//!
//! `#[derive(Model)]` (via `model::cacheable::expand` — when `Cacheable`
//! applies, i.e. `pk` ≠ `None`) emits one `inventory::submit!` per
//! model, each containing a `SassiBootHook` whose `fn(&mut Sassi)`
//! constructs a `Punnu<T>` and registers it on the orchestrator.
//!
//! `DjogiContext::from_pool` (and any other top-level constructor)
//! walks `inventory::iter::<SassiBootHook>()` once with a fresh
//! `&mut Sassi`, then freezes into `Arc<Sassi>` and stores it on the
//! context. After boot the registry is read-only.
//!
//! Cross-context behaviour: each top-level `DjogiContext` builds its
//! own `Sassi`. `begin()` / `atomic()` SHARE the parent's `Arc<Sassi>`
//! (cache state is transaction-scope-agnostic). This is the "DjogiContext
//! IS the tenant boundary" contract from cluster 8δ T7.4.

/// A boot-time hook that registers one `Punnu<T>` on a `Sassi` orchestrator.
///
/// Every `#[model]` struct (with the exception of `pk = None`) emits an
/// `inventory::submit!` of a `SassiBootHook` — a thin newtype around
/// `fn(&mut Sassi)`. `DjogiContext::from_pool` walks the inventory once
/// and applies every hook so the context's `Sassi` starts with a pool
/// registered for each `Cacheable` model type in the current binary.
///
/// Macro-emitted code spells this as `::djogi::SassiBootHook` (re-exported
/// at the crate root) per `feedback_macro_path_routing.md`.
pub struct SassiBootHook(pub fn(&mut sassi::Sassi));

inventory::collect!(SassiBootHook);
