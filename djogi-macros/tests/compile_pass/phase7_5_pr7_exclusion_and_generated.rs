// Phase 7.5 PR 7 task 2 — `#[model(exclusion(...))]` + `#[field(generated = "...")]`.
//
// Exercises the macro's parse + lower path for the new descriptor knobs:
//
// 1. A model with one `exclusion(...)` declaration covering every knob:
//    `name`, `using`, multi-element list, `where`, `deferrable`,
//    `initially_deferred`. Asserts the parser accepts the full surface.
// 2. A model with multiple `exclusion(...)` entries — the IR is a `Vec`
//    that accumulates each entry independently.
// 3. A model with `#[field(generated = "<expr>")]`. `stored: true` is
//    implicit; the macro hard-codes it at lowering time per Pg18.
// 4. A combined model that mixes both surfaces — proves they compose.
//
// `no_default` because `time::Date` and `time::OffsetDateTime` (the
// underlying `Date` / `DateTime` aliases the model would otherwise
// require) do not implement `Default`. The fixture's purpose is to
// exercise the macro's parse + descriptor-emit pipeline; the runtime
// behaviour of the resulting models is out of scope here (PR 7 task 4
// covers DDL emission, task 6 covers live integration).

use djogi::prelude::*;

// ── (1) Single exclusion(...) covering every knob ───────────────────────

#[model(
    table = "bookings_full",
    no_default,
    exclusion(
        name = "no_overlap",
        using = "gist",
        elements = ["room_id WITH =", "period WITH &&"],
        where = "is_active",
        deferrable = true,
        initially_deferred = true,
    ),
)]
#[derive(Debug, Clone)]
pub struct BookingFull {
    pub room_id: i64,
    pub period: String,
    pub is_active: bool,
}

// ── (2) Multiple exclusion(...) entries on one model ────────────────────

#[model(
    table = "bookings_multi",
    no_default,
    exclusion(
        name = "no_room_overlap",
        using = "gist",
        elements = ["room_id WITH =", "period WITH &&"],
    ),
    exclusion(
        name = "unique_per_tenant",
        using = "btree",
        elements = ["tenant_id WITH =", "external_id WITH ="],
    ),
)]
#[derive(Debug, Clone)]
pub struct BookingMulti {
    pub room_id: i64,
    pub period: String,
    pub tenant_id: i64,
    pub external_id: String,
}

// ── (3) `#[field(generated = "<expr>")]` ────────────────────────────────

#[model(table = "users_generated", no_default)]
#[derive(Debug, Clone)]
pub struct UserGenerated {
    pub email: String,
    #[field(generated = "LOWER(email)")]
    pub email_lower: String,
}

// ── (4) Combined: exclusion(...) + generated field on one model ─────────

#[model(
    table = "bookings_combined",
    no_default,
    exclusion(
        name = "no_overlap_combined",
        using = "gist",
        elements = ["room_id WITH =", "period WITH &&"],
    ),
)]
#[derive(Debug, Clone)]
pub struct BookingCombined {
    pub room_id: i64,
    pub period: String,
    pub email: String,
    #[field(generated = "LOWER(email)")]
    pub email_lower: String,
}

fn main() {}
