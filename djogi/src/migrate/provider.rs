//! The descriptor-provider boundary (#370).
//! Descriptor-dependent CLI commands (`compose`/`verify`/`schema`/`docs`)
//! need the `#[derive(Model)]` descriptors compiled into the *running*
//! binary. The published standalone `djogi` binary links no adopter
//! model crates, so reading the global `inventory` registry directly
//! yields zero adopter models. Injecting a [`DescriptorProvider`] lets an
//! adopter-linked binary supply its own models in place of an empty
//! inventory, while the published binary keeps today's behavior via
//! [`InventoryDescriptorProvider`].

use crate::apps::{AppDescriptor, AppRegistry};
use crate::descriptor::{DeferrabilitySpec, EnumDescriptor, ModelDescriptor};

/// Source of the `#[derive(Model)]` descriptors a descriptor-dependent
/// command operates on.
/// # What
/// Supplies the four descriptor streams the migration projection +
/// docs renderer consume: models, enums, apps, and FK-deferrability
/// sidecars.
/// # Why
/// Injecting this is what lets an adopter-linked `djogi` binary supply
/// its own models in place of `djogi-cli`'s empty inventory (the
/// standalone published binary links no model crates). It is the
/// boundary between "djogi works in its own test suite" and "djogi
/// works for someone who `cargo add djogi`s it".
/// # How
/// The only shipped implementation is [`InventoryDescriptorProvider`],
/// which reads the link-time `inventory` registry — exactly the
/// descriptors compiled into the calling binary. Pass it to
/// [`super::project_from_provider`] /
/// [`super::generate_docs_with_provider`].
/// # Where
/// `djogi_cli::run_with_provider` threads a `&dyn DescriptorProvider`
/// to the per-command functions that need it. Adopters do not implement
/// this trait — they link their model crates and the inventory provider
/// sees them.
/// # Trust
/// This trait is intentionally **open** (not sealed) so tests and future
/// providers (e.g. a fixture provider returning a fixed model set) may
/// implement it. A custom provider is **trusted, framework-shaped
/// code**, not an untrusted-input surface: it only supplies descriptors
/// to the adopter's *own* CLI invocation, where the adopter already
/// controls the linked code. It cannot reach or corrupt any other
/// process's state.
/// # Boundary completeness
/// `project_from_provider` reads models/enums/apps/deferrability through
/// this trait, so a non-inventory provider yields a *complete*
/// projection. The relation-accessor collision gate
/// (`ReverseRelationMarker`) is a link-time validation of the binary's
/// Rust accessors — not descriptor data — and stays an ambient pre-gate
/// inside `project_from_provider`, documented as the one deliberate
/// ambient read.
pub trait DescriptorProvider {
    /// Model descriptors visible to this process.
    fn models(&self) -> Vec<&'static ModelDescriptor>;

    /// Enum descriptors visible to this process.
    fn enums(&self) -> Vec<&'static EnumDescriptor>;

    /// App descriptors, INCLUDING the synthetic global bucket, sorted,
    /// with app-identity uniqueness validated — i.e. the same contract
    /// as [`AppRegistry::all`], not a bare `inventory::iter`.
    /// # Panics
    /// The existing inventory implementation delegates to [`AppRegistry::all`],
    /// which panics on first call if two app descriptors collide on
    /// `label` (the workspace-wide label-uniqueness invariant).
    fn apps(&self) -> &'static [AppDescriptor];

    /// FK-deferrability sidecars. The projection reads these to build the
    /// per-field `(deferrable, initially_deferred)` map; without it on
    /// the trait, a non-inventory provider's projection would silently
    /// fall back to ambient globals and be incomplete.
    fn deferrability_specs(&self) -> Vec<&'static DeferrabilitySpec>;
}

/// The link-time inventory-backed [`DescriptorProvider`] — the only
/// shipped implementation.
/// # What
/// Reads the `inventory` registry: exactly the `#[derive(Model)]` /
/// `#[derive(DjogiEnum)]` / `djogi::apps!` items compiled into the
/// calling binary.
/// # Why
/// Reproduces today's CLI behavior under dependency injection. The
/// published standalone `djogi` binary uses it and sees zero adopter
/// models; an adopter-linked binary uses the same type and sees its own
/// models, because they are linked into *that* binary's inventory.
/// # How
/// ```no_run
/// use djogi::migrate::{DescriptorProvider, InventoryDescriptorProvider};
/// let provider = InventoryDescriptorProvider::new();
/// let models = provider.models(); // every #[derive(Model)] in this binary
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct InventoryDescriptorProvider;

impl InventoryDescriptorProvider {
    /// Construct the inventory-backed provider. Zero-sized; cheap.
    pub fn new() -> Self {
        Self
    }
}

impl DescriptorProvider for InventoryDescriptorProvider {
    fn models(&self) -> Vec<&'static ModelDescriptor> {
        inventory::iter::<ModelDescriptor>().collect()
    }

    fn enums(&self) -> Vec<&'static EnumDescriptor> {
        inventory::iter::<EnumDescriptor>().collect()
    }

    fn apps(&self) -> &'static [AppDescriptor] {
        // AppRegistry::all() = global bucket + sort + identity-uniqueness
        // validation (apps.rs:301), NOT a bare inventory::iter.
        AppRegistry::all()
    }

    fn deferrability_specs(&self) -> Vec<&'static DeferrabilitySpec> {
        inventory::iter::<DeferrabilitySpec>().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_provider_apps_include_global_bucket() {
        let p = InventoryDescriptorProvider::new();
        // AppRegistry::all() always prepends the synthetic global bucket
        // (label ""), so apps() is never empty even with no #[app] decls.
        let apps = p.apps();
        assert!(
            apps.iter()
                .any(|a| a.label == crate::apps::AppDescriptor::GLOBAL_LABEL),
            "InventoryDescriptorProvider::apps() must include the synthetic global bucket"
        );
    }

    #[test]
    fn inventory_provider_is_object_safe() {
        // Compile-time proof the trait is object-safe (the entrypoints
        // take `&dyn DescriptorProvider`).
        let p = InventoryDescriptorProvider::new();
        let _dyn_ref: &dyn DescriptorProvider = &p;
    }

    #[allow(clippy::default_constructed_unit_structs)]
    #[test]
    fn default_matches_new() {
        let _a = InventoryDescriptorProvider::default();
        let _b = InventoryDescriptorProvider::new();
    }
}
