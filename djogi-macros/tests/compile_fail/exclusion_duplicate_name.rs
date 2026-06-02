// two `exclusion(...)` entries with the same `name`
// on the same model are rejected at parse time.
//
// Postgres constraint names live in the per-table namespace and must be
// unique; the macro enforces the same rule at compile time so the
// collision surfaces with a span-precise diagnostic instead of an
// opaque `pg_constraint`-level failure on first migration apply.
use djogi::prelude::*;

#[model(
    table = "bookings",
    no_default,
    exclusion(
        name = "no_overlap",
        using = "gist",
        elements = ["room_id WITH =", "period WITH &&"],
    ),
    exclusion(
        name = "no_overlap",
        using = "btree",
        elements = ["tenant_id WITH ="],
    ),
)]
#[derive(Debug, Clone)]
pub struct Booking {
    pub room_id: i64,
    pub period: String,
    pub tenant_id: i64,
}

fn main() {}
