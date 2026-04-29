// Phase 7.5 PR 7 — `exclusion(...)` requires `name = "..."`.
//
// Every EXCLUDE constraint must carry an explicit name (the macro does
// not invent one because adopters typically reference the constraint by
// name in `ALTER TABLE ... DROP CONSTRAINT` follow-ups). Omitting `name`
// triggers a span-precise error pointing at the `exclusion(` head.
use djogi::prelude::*;

#[model(
    table = "bookings",
    no_default,
    exclusion(
        using = "gist",
        elements = ["room_id WITH =", "period WITH &&"],
    ),
)]
#[derive(Debug, Clone)]
pub struct Booking {
    pub room_id: i64,
    pub period: String,
}

fn main() {}
