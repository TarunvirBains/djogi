//! `HerdSummary` — a hand-rolled `Herd` projection plus a side-query
//! trait for the live elephant count.
//!
//! ## Why hand-rolled
//!
//! Djogi can auto-generate visages for any `#[field(expose(public))]`
//! annotation, naming the result `{Source}Public` (here, `HerdPublic`).
//! That naming convention is fixed; when adopters want a domain-specific
//! visage name, they write the visage as a plain struct with a
//! `From<&Source>` impl, which is exactly what this file does.
//!
//! ## The side-query pattern
//!
//! Naive approach: add `size: i32` to `Herd`, denormalise from
//! `Elephant` on every elephant insert / delete. Now every `Herd` write
//! is contended and the count drifts under concurrent edits.
//!
//! Visage approach: the projection lists only the cheap columns
//! (`id`, `name`, `estimated_population`). For the expensive aggregate
//! (`herd_size` —  `COUNT(*) FROM elephants WHERE herd_id = $1`), the
//! visage implements a trait that runs the side query when the caller
//! actually asks for it.
//!
//! Result: visage selects stay narrow; the size query runs only when
//! callers request it; no denormalisation drift.

use crate::models::Herd;
use djogi::prelude::*;

/// Cheap projection of [`Herd`]. Constructed via `HerdSummary::from(&herd)`
/// — no DB round-trip beyond whatever loaded the source row.
#[derive(Debug, Clone)]
pub struct HerdSummary {
    pub id: HeerId,
    pub name: String,
    pub estimated_population: i32,
}

impl From<&Herd> for HerdSummary {
    fn from(h: &Herd) -> Self {
        HerdSummary {
            id: h.id,
            name: h.name.clone(),
            estimated_population: h.estimated_population,
        }
    }
}

/// Side-query trait. The trait exists so callers can pass
/// `&dyn HerdSizeQuery` into helpers without coupling to the visage
/// type — the obvious composition surface for "fetch this aggregate
/// off any visage that has a herd id."
///
/// The method takes `&mut DjogiContext` because `DjogiContext` is not
/// `Sync` — every Djogi query path threads a unique mutable handle so
/// pool checkouts and active transactions remain unambiguous.
pub trait HerdSizeQuery {
    /// Live count of elephants in the herd. One round-trip; the caller
    /// chooses when to call it.
    fn herd_size(
        &self,
        ctx: &mut DjogiContext,
    ) -> impl std::future::Future<Output = Result<i64, DjogiError>>;
}

impl HerdSizeQuery for HerdSummary {
    async fn herd_size(&self, ctx: &mut DjogiContext) -> Result<i64, DjogiError> {
        // Raw scalar via the always-available escape hatch. Adopters
        // could also reach for `Elephant::objects().filter(...).count()`
        // once they've bound an `Elephant` model in scope; the raw form
        // keeps this trait method's intent unambiguous in isolation.
        ctx.raw_scalar(
            "SELECT COUNT(*)::BIGINT FROM elephants WHERE herd_id = $1",
            &[&self.id],
        )
        .await
    }
}
