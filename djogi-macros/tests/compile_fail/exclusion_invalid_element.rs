// `elements` entry without the ` WITH ` delimiter is
// rejected at parse time.
//
// Every element string must look like `"<expr> WITH <op>"` (uppercase,
// single space on each side). Bare column references without a
// comparison operator are a malformed exclusion specification because
// Postgres requires the operator class member explicitly.
use djogi::prelude::*;

#[model(
    table = "bookings",
    no_default,
    exclusion(
        name = "no_overlap",
        using = "gist",
        elements = ["room_id"],
    ),
)]
#[derive(Debug, Clone)]
pub struct Booking {
    pub room_id: i64,
}

fn main() {}
