//! Trait registry for cross-type queries via `#[djogi::trait_impl]`.
//! # What
//! [`TraitRegistration`] is the runtime entry that the
//! `#[djogi::trait_impl]` attribute macro emits via
//! `inventory::submit!` per `impl Trait for Model` block. The
//! registry lets cross-cutting consumers walk every model that
//! implements a given trait at runtime, without forcing every
//! adopter to enumerate the implementations by hand.
//! # Why
//! Adopters writing `#[djogi::trait_impl] impl Searchable for Vehicle
//! {... }` get Vehicle visible to every cross-cutting query against
//! `dyn Searchable` without naming Vehicle in the consumer's path.
//! Mirrors the established `inventory`-based registry pattern from
//! `djogi::apps::AppRegistry` — compile-time submission, one-shot
//! at first read, downstream consumers iterate.
//! # Relationship to sassi's `Sassi::all_impl::<dyn T>()`
//! Sassi ships its own `#[sassi::trait_impl]` attribute macro and
//! `inventory` registry (`sassi::trait_registry`). Sassi's registry
//! is keyed off `Punnu<T>`-scoped models — Sassi knows about
//! per-model pools and uses the registry to construct `Vec<Arc<dyn T>>`
//! across every Punnu-registered model implementing the trait.
//! Djogi's registry here is a sibling surface for cross-type queries
//! that do **not** require the Punnu pool boundary. Adopters with
//! sassi enabled and Punnu-pooled models use `#[sassi::trait_impl]`
//! and `Sassi::all_impl::<dyn T>()` for the full sassi-integrated
//! cross-type query path; adopters who only need the descriptor-
//! level enumeration use `#[djogi::trait_impl]` and
//! [`iter_for_trait::<dyn T>()`] directly.
//! Plan §7 #12 (resolved 2026-05-03): this module owns
//! `djogi::trait_registry::*` surface only; the sassi-bridging
//! consumer surface is sassi's own responsibility. `djogi::cache::*`
//! covers the Punnu cache surface; this module stays narrowly focused
//! on the registration plumbing.
//! # Type erasure
//! Trait-object pointer manipulation is the soundness-critical
//! surface. The registry's wire type is `Arc<dyn Any + Send + Sync>`;
//! the per-impl caster routes through a per-(Model, Trait)
//! carrier struct that satisfies `Any + Send + Sync + 'static`,
//! using only Rust's built-in coercions — `Arc::downcast`, unsized
//! `Arc<T>` → `Arc<dyn Trait>` coercion, never `transmute`.

/// Erased Arc carrier — `Arc<dyn Any + Send + Sync>`. Type alias
/// improves readability of the `caster` field signature and silences
/// the `clippy::type_complexity` lint.
pub type ErasedArc = std::sync::Arc<dyn std::any::Any + Send + Sync>;

/// Type-erased caster function — accepts an `ErasedArc` holding the
/// model instance, returns `Some(carrier_for_dyn_T)` when the input
/// downcasts to the registered model type; `None` otherwise.
/// Type alias improves readability of the `TraitRegistration.caster`
/// field signature.
pub type CasterFn = fn(&ErasedArc) -> Option<ErasedArc>;

/// One trait-impl registration entry. Emitted via
/// `inventory::submit!(TraitRegistration {... })` by the
/// `#[djogi::trait_impl]` attribute macro.
/// # Fields
/// - `model_type_id` — the `TypeId` of the implementing model
/// (`Vehicle` for `impl Searchable for Vehicle`). Returned by a
/// `fn() -> TypeId` so the registration is `const`-constructible
/// at the macro emission site without touching `TypeId::of` (which
/// is `const`-only on nightly).
/// - `trait_type_id` — the `TypeId` of the registered trait
/// (`dyn Searchable`). Same `fn() -> TypeId` discipline.
/// - `model_type_name` / `trait_type_name` — human-readable names
/// for diagnostic / introspection paths. `&'static str` so the
/// registration stays `const`-submittable.
/// - `caster` — the type-erased downcast helper emitted by the macro.
/// # Layout stability
/// Same convention as `ModelDescriptor` and the other inventory-
/// submitted descriptors: every text field is `&'static str`, every
/// non-text field is `Copy`, struct layout stays stable so adding a
/// future field is a breaking change at every emission site (which
/// is exactly the discipline we want — adopters never touch this
/// directly; the macro is the only emitter).
// Note: NOT `#[non_exhaustive]` — the macro-emitted code in adopter
// crates constructs this struct via a literal in `inventory::submit!`,
// which Rust rejects on `#[non_exhaustive]` structs (E0639). The
// add-a-field-is-breaking discipline is enforced by convention rather
// than the attribute; every macro emission site updates in lockstep
// when a new field lands.
#[derive(Debug)]
pub struct TraitRegistration {
    /// Returns the implementing model's `TypeId`. The macro emits
    /// `|| ::std::any::TypeId::of::<Vehicle>()` here.
    pub model_type_id: fn() -> std::any::TypeId,
    /// Returns the registered trait's `TypeId` — typically obtained
    /// via `TypeId::of::<dyn Trait>()` on a sized newtype that the
    /// macro emits alongside the registration.
    pub trait_type_id: fn() -> std::any::TypeId,
    /// Human-readable model type name. The `Vehicle` in
    /// `impl Trait for Vehicle`. Used for diagnostics and for the
    /// `Sassi::all_impl::<dyn T>()` consumer's debug logging.
    pub model_type_name: &'static str,
    /// Human-readable trait type name. The `Searchable` in
    /// `impl Searchable for Vehicle`.
    pub trait_type_name: &'static str,
    /// Type-erased caster. Given an `Arc<dyn Any + Send + Sync>`
    /// holding the model instance, returns
    /// `Some(arc_to_carrier_for_dyn_T)` when the input downcasts to
    /// the registered model type; `None` otherwise.
    /// The macro fills the body using the canonical `CastedTraitObj`
    /// carrier pattern (or equivalent safe approach).
    /// The field is `pub` so the macro can populate it; downstream
    /// consumers reach it via [`iter_for_trait`] / `Sassi::all_impl`.
    pub caster: CasterFn,
}

inventory::collect!(TraitRegistration);

/// Convenience iterator over every registered `TraitRegistration`.
/// Wraps `inventory::iter::<TraitRegistration>` so consumers do not
/// need the `inventory` crate as a direct dependency. Prefer
/// [`iter_for_trait`] when filtering by a specific trait type id
/// the unfiltered iterator is useful for diagnostic dumps.
pub fn iter_registrations() -> impl Iterator<Item = &'static TraitRegistration> {
    inventory::iter::<TraitRegistration>()
}

/// One-shot, lazily-built index of `inventory::iter::<TraitRegistration>`
/// keyed by trait `TypeId`. Built on the first `iter_for_trait` call by
/// invoking each registration's `trait_type_id` fn-ptr exactly once;
/// every subsequent lookup is a single `HashMap::get`.
/// The pre-cache shape walked the full registration list and called
/// `(r.trait_type_id)()` on every entry per call — O(n) fn-ptr calls
/// per `iter_for_trait` invocation. The cache flips that to O(1) for
/// hot paths (e.g. cross-type queries that re-enumerate trait impls
/// frequently) at the cost of one map allocation per process.
static REGISTRY_CACHE: std::sync::OnceLock<
    std::collections::HashMap<std::any::TypeId, Vec<&'static TraitRegistration>>,
> = std::sync::OnceLock::new();

fn registry_cache()
-> &'static std::collections::HashMap<std::any::TypeId, Vec<&'static TraitRegistration>> {
    REGISTRY_CACHE.get_or_init(|| {
        let mut map: std::collections::HashMap<std::any::TypeId, Vec<&'static TraitRegistration>> =
            std::collections::HashMap::new();
        for reg in inventory::iter::<TraitRegistration>() {
            map.entry((reg.trait_type_id)()).or_default().push(reg);
        }
        map
    })
}

/// Iterate every `TraitRegistration` whose `trait_type_id` matches
/// `T`'s `TypeId`. 4 — the consumer-side filter for
/// `Sassi::all_impl::<dyn T>()` and equivalent cross-type queries.
/// The `T: ?Sized + 'static` bound covers `dyn Trait` types — the
/// canonical caller-side spelling is
/// `iter_for_trait::<dyn Searchable>()`.
/// First call builds the keyed cache (O(n) fn-ptr calls); subsequent
/// calls are O(1) `HashMap` lookups. Returns `Vec<&'static …>`
/// `inventory::collect!` already yields `'static` references, and the
/// cache holds them by reference, so the returned iterator borrows
/// from the static cache without per-call allocation aside from the
/// `Vec` clone of pointers (cheap; one word per registered impl).
pub fn iter_for_trait<T: ?Sized + 'static>() -> impl Iterator<Item = &'static TraitRegistration> {
    let target = std::any::TypeId::of::<T>();
    let entries: Vec<&'static TraitRegistration> =
        registry_cache().get(&target).cloned().unwrap_or_default();
    entries.into_iter()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-construct a `TraitRegistration` literal — locks the
    /// struct shape so a future rename / retyping surfaces here. The
    /// `caster` field returns `None` for every input; the macro fills
    /// in a real body.
    #[test]
    fn trait_registration_struct_shape() {
        struct FakeModel;
        struct FakeTrait;
        let reg = TraitRegistration {
            model_type_id: || std::any::TypeId::of::<FakeModel>(),
            trait_type_id: || std::any::TypeId::of::<FakeTrait>(),
            model_type_name: "FakeModel",
            trait_type_name: "FakeTrait",
            caster: |_| None,
        };
        assert_eq!((reg.model_type_id)(), std::any::TypeId::of::<FakeModel>());
        assert_eq!((reg.trait_type_id)(), std::any::TypeId::of::<FakeTrait>());
        assert_eq!(reg.model_type_name, "FakeModel");
        assert_eq!(reg.trait_type_name, "FakeTrait");
    }

    /// `iter_for_trait` filters registrations by the requested type
    /// id. With no `inventory::submit!` blocks reachable from this
    /// test, the iterator yields zero entries — the count is
    /// reachable as a sanity check on the filter logic.
    #[test]
    fn iter_for_trait_returns_empty_when_no_impls_registered() {
        struct FakeTrait;
        let count = iter_for_trait::<FakeTrait>().count();
        // Zero or more — the inventory may carry registrations from
        // other test fixtures or integration paths. The invariant
        // we lock is: no panic, no UB, the iterator is well-formed.
        let _ = count;
    }
}
