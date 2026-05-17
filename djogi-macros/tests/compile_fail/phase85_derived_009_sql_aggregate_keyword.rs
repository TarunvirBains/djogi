//! Phase 8.5 #231 — E_DJG_VDF_009: derived `sql` references an
//! aggregate or window construct.
//!
//! `validate_sql_surface` in `djogi-macros/src/model/derived.rs`
//! walks the SQL through `aggregate_or_over_hit`, which detects
//! `<aggregate_name>(` followed by an opening paren, or the bare
//! `OVER` window keyword. Either trips E_DJG_VDF_009 at parse time
//! — derived expressions are per-row scalars; aggregates and window
//! functions belong to a future `#[annotation]` attribute.

use djogi::prelude::*;

#[model(table = "phase85_e009_consignments")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = total_sites,
    ty     = i64,
    scopes = [public],
    sql    = "COUNT(inbound_site)",
    rust   = "1i64",
)]
pub struct Consignment {
    #[field(expose(public))]
    pub inbound_site: String,
}

fn main() {}
