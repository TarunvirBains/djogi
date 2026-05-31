use crate::apps::{AppDescriptor, AppRegistry};
use crate::descriptor::ModelDescriptor;

/// Provides the set of model descriptors for migration projection.
///
/// Implementors supply (app-label, descriptor) pairs.
/// The default implementation reads from the compiled-in `inventory` registry.
/// Custom implementations allow tests and external tooling to inject
/// synthetic descriptor sets without touching global state.
pub trait DescriptorProvider {
    /// Return all registered model descriptors grouped by application label.
    fn descriptors(&self) -> Vec<(&str, Vec<&'static ModelDescriptor>)>;
}

/// Reads descriptors from the `inventory` registry populated by
/// `#[derive(Model)]` at compile time.
///
/// This is the production default. The `apps()` method delegates to
/// [`AppRegistry::all`] to obtain the global bucket plus identity-unique,
/// sorted application slice.
pub struct InventoryDescriptorProvider;

impl DescriptorProvider for InventoryDescriptorProvider {
    fn descriptors(&self) -> Vec<(&str, Vec<&'static ModelDescriptor>)> {
        let apps = AppRegistry::all();
        let mut result: Vec<(&str, Vec<&'static ModelDescriptor>)> = Vec::new();

        // Global (unnamed) bucket from inventory
        let global: Vec<&'static ModelDescriptor> = inventory::iter::<ModelDescriptor>()
            .filter(|d| d.app == Some(AppDescriptor::GLOBAL_LABEL))
            .collect();
        result.push((AppDescriptor::GLOBAL_LABEL, global));

        // Per-app buckets
        for app in apps {
            let models: Vec<&'static ModelDescriptor> = inventory::iter::<ModelDescriptor>()
                .filter(|d| d.app == Some(app.label))
                .collect();
            result.push((app.label, models));
        }

        result
    }
}
