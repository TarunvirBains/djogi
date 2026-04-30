//! `HerdSummary` — a `Herd` projection that exposes herd size through a
//! side-query trait rather than baking it into the row.
//!
//! ## The pattern
//!
//! Naive approach: add `size: i32` to `Herd`, denormalize from `Elephant`
//! on every elephant insert/delete. Now every `Herd` write is contended
//! and the count drifts under concurrent edits.
//!
//! Visage approach: the projection lists only the *cheap* columns
//! (`id`, `name`, `estimated_population`). For the *expensive* aggregate
//! (`herd_size` — `COUNT(*) FROM elephants WHERE herd_id = $1`), the
//! visage implements a trait that runs the side query when the caller
//! actually asks for it.
//!
//! Result: visage selects stay narrow; the size query is one round-trip
//! when (and only when) the caller wants it; no denormalization drift.

use djogi::prelude::*;
use crate::models::Herd;

/// Projection over `Herd` exposed through `#[model(visages = "...")]`.
/// The `expose` grammar lives on the `Herd` declaration when the example
/// is wired up — this struct mirrors what the macro emits.
#[derive(Debug, Clone)]
pub struct HerdSummary {
    pub id: HeerId,
    pub name: String,
    pub estimated_population: i32,
}

/// Side-query trait. The visage exposes a method that hits the database
/// for the herd_size aggregate instead of carrying it as a column.
///
/// Implementors are typically just the visage struct; the trait exists
/// so callers can pass `&dyn HerdSizeQuery` into helpers without coupling
/// to the visage type.
pub trait HerdSizeQuery {
    /// Live count of elephants in the herd. One round-trip; the caller
    /// is responsible for choosing when to call it.
    fn herd_size(
        &self,
        ctx: &DjogiContext,
    ) -> impl std::future::Future<Output = Result<i64, DjogiError>> + Send;
}

impl HerdSizeQuery for HerdSummary {
    async fn herd_size(&self, ctx: &DjogiContext) -> Result<i64, DjogiError> {
        // `Elephant::objects().filter(...).count()` is the obvious shape.
        // Wired up against real Djogi APIs once cluster PRs land.
        todo!("wire against Elephant::objects().filter(herd_id = self.id).count()")
    }
}
