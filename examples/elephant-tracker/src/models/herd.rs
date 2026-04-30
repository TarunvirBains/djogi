//! Herd — a named family group of elephants.
//!
//! Demonstrates:
//! - M2M to `Country` through the explicit `HerdRange` model. Djogi
//!   does not provide implicit M2M fields — every M2M is an explicit
//!   through model with whatever payload the relationship needs.
//! - `const RELATION` on the M2M relation — the method name comes from
//!   here, not from auto-pluralization.
//!
//! The herd's primary range — country it spends the most time in —
//! is *not* on this model; it's derived from `HerdRange.season` rows
//! and computed in the `cross_border_herds` demo.

use djogi::prelude::*;

#[djogi::model(table = "herds")]
#[derive(Debug, Clone)]
pub struct Herd {
    /// Display name. Unique within an organization in a real app;
    /// for the example the schema is laxer.
    #[field(unique)]
    pub name: String,

    /// Estimated population at last census. The visage layer uses
    /// `herd_size` (a side query against `Elephant`) to surface the
    /// real-time count without baking it into this row.
    pub estimated_population: i32,
}

impl Herd {
    /// M2M relation method name comes from here, not from
    /// auto-pluralization. `herd.countries(ctx).fetch_all()` or similar.
    pub const COUNTRIES_RELATION: &'static str = "countries";
}
