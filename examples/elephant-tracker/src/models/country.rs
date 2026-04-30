//! Country — the simplest model in the example.
//!
//! ## What this demonstrates
//!
//! - `pk = Serial` — a small reference table with a 32-bit serial PK rather
//!   than a HeerId. Use this for lookup tables that never need cross-node
//!   ID generation: countries, fuel types, status codes, currencies, etc.
//! - `#[field(unique)]` — column-level uniqueness; the migration differ
//!   lowers it to a `UNIQUE` constraint in Postgres DDL.
//! - `#[field(max_length = 3)]` — caps the underlying `TEXT` column at
//!   `VARCHAR(3)`.
//!
//! Five rows total (Kenya, Tanzania, Uganda, Botswana, Zimbabwe). Seeded
//! by hand-written SQL in `seeds/countries.sql` because the data is tiny,
//! static, and human-curated.

use djogi::prelude::*;

#[model(table = "countries", pk = Serial)]
#[derive(Debug, Clone)]
pub struct Country {
    /// ISO 3166-1 alpha-3 code. Unique per row.
    #[field(unique, max_length = 3)]
    pub iso_alpha3: String,

    /// Display name in English. Not unique — two countries could share a
    /// name in principle, and the example refuses to bake politics into
    /// the schema.
    pub name: String,
}
