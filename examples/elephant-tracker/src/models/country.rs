//! Country — the simplest model in the example.
//!
//! Demonstrates: `pk = "serial"` for small reference tables, `no_default`
//! for `OffsetDateTime` fields the application sets, and Djogi's
//! "models can be tiny" stance.
//!
//! Five rows total (Kenya, Tanzania, Uganda, Botswana, Zimbabwe). Seeded
//! by hand-written SQL in `seeds/countries.sql`.

use djogi::prelude::*;

#[djogi::model(table = "countries", pk = "serial")]
#[derive(Debug, Clone)]
pub struct Country {
    /// ISO 3166-1 alpha-3 code. Unique per row.
    #[field(unique, max_length = 3)]
    pub iso_alpha3: String,

    /// Display name in English. Not unique — two countries could share
    /// a name in principle (`Congo`, `Congo`), and the example refuses
    /// to bake politics into the schema.
    pub name: String,
}
