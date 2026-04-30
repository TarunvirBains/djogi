// Phase 7.5 PR 7 — `exclusion(...)` requires `using = "..."`.
//
// The index method is required because Postgres lacks a default
// (`EXCLUDE USING` syntax mandates an index method). Common values are
// `"gist"` for range overlap and `"btree"` for `=`-based exclusion.
use djogi::prelude::*;

#[model(
    table = "bookings",
    no_default,
    exclusion(
        name = "no_overlap",
        elements = ["room_id WITH =", "period WITH &&"],
    ),
)]
#[derive(Debug, Clone)]
pub struct Booking {
    pub room_id: i64,
    pub period: String,
}

fn main() {}
