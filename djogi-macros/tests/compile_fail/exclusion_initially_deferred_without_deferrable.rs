// `initially_deferred = true` requires
// `deferrable = true` on the same `exclusion(...)` entry.
//
// `INITIALLY DEFERRED` is meaningless on a non-deferrable constraint
// (Postgres has nothing to defer to the end of the transaction). The
// macro rejects the inconsistent pairing at parse time so the error
// surfaces at the attribute, not at first DDL emission.
use djogi::prelude::*;

#[model(
    table = "bookings",
    no_default,
    exclusion(
        name = "no_overlap",
        using = "gist",
        elements = ["room_id WITH =", "period WITH &&"],
        initially_deferred = true,
    ),
)]
#[derive(Debug, Clone)]
pub struct Booking {
    pub room_id: i64,
    pub period: String,
}

fn main() {}
