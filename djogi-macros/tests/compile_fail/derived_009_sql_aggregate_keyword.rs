//! E_DJG_VDF_009: derived `sql` references an
//! aggregate or window construct.
//!
//! `validate_sql_surface` in `djogi-macros/src/model/derived.rs`
//! walks the SQL through `aggregate_or_over_hit`, which detects
//! `<aggregate_name>(` followed by an opening paren, or the bare
//! `OVER` window keyword. Either trips E_DJG_VDF_009 at parse time
//! — derived expressions are per-row scalars and Tier 1 rejects
//! aggregates / window functions in `#[derived]` `sql` today. The
//! future aggregate / window surface is locked but not yet
//! implemented: Shape Q (QuerySet `.annotate(...)`) and Shape V
//! (`#[derived(..., aggregate = true)]`); the `aggregate = true`
//! marker is not accepted by the parser yet. See the
//! aggregate-annotation declaration-site decision in
//! `docs/spec/decisions.md`.

use djogi::prelude::*;

#[model(table = "phase85_e009_consignments")]
#[derive(Model, Debug, Clone)]
#[derived(
 name = total_sites,
 ty  = i64,
 scopes = [public],
 sql = "COUNT(inbound_site)",
 rust = "1i64",
)]
pub struct Consignment {
 #[field(expose(public))]
 pub inbound_site: String,
}

fn main() {}
