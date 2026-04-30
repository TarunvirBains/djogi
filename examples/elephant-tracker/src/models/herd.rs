//! Herd — a named family group of elephants.
//!
//! ## What this demonstrates
//!
//! - The `Herd` side of a many-to-many relationship to `Country` through
//!   the explicit `HerdRange` model. Djogi does not provide implicit M2M
//!   fields — every M2M is an explicit through model with whatever
//!   payload the relationship needs. The macro invocation lives in
//!   `mod.rs` because `many_to_many!` takes bare type identifiers.
//!
//! Adopters write `herd.countries(ctx).await` for the M2M side; they
//! construct `HerdSummary::from(&herd)` to get a hand-rolled projection
//! that exposes a `herd_size` side-query (see `crate::visages`).

use djogi::prelude::*;

#[model(table = "herds", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Herd {
    /// Display name. Unique within an organization in a real app;
    /// the example keeps the schema laxer.
    #[field(unique)]
    pub name: String,

    /// Estimated population at last census. The `HerdSummary` visage's
    /// `herd_size` side-query method surfaces the live count from
    /// `Elephant` without denormalising it onto this row.
    pub estimated_population: i32,
}
