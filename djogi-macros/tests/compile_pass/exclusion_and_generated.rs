// `#[model(exclusion(...))]` + `#[field(generated = "...")]`.
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
// 5. A pure-range exclusion (`period WITH &&` only) — proves the
//    djogi#148 auto-derivation does NOT request `btree_gist` for
//    range-only EXCLUDEs (stock GiST handles overlap natively).
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

// ── (5) Pure-range exclusion — djogi#148 negative case ──────────────────
//
// `period WITH &&` works with stock GiST; the macro must NOT auto-derive
// `btree_gist` for this shape. The descriptor-emit smoke test below
// proves the spec lands with `extension_dependency: None`.

#[model(
    table = "calendar_slots",
    no_default,
    exclusion(
        name = "no_period_overlap",
        using = "gist",
        elements = ["period WITH &&"],
    ),
)]
#[derive(Debug, Clone)]
pub struct CalendarSlot {
    pub period: String,
}

fn main() {
    // Compile-only descriptor inspection: assert that the auto-derived
    // `extension_dependency` slots are populated as expected.
    //
    // BookingFull combines `room_id WITH =` + `period WITH &&` → must
    // request btree_gist (djogi#148 auto-derive).
    let booking_specs = <BookingFull as Model>::descriptor().exclusion_constraints;
    assert_eq!(booking_specs.len(), 1);
    assert_eq!(booking_specs[0].extension_dependency, Some("btree_gist"));

    // BookingMulti has one `using = "gist"` (`room_id WITH =` →
    // btree_gist) and one `using = "btree"` (always None, regardless of
    // operators). Both auto-derivations land independently.
    let multi_specs = <BookingMulti as Model>::descriptor().exclusion_constraints;
    assert_eq!(multi_specs.len(), 2);
    // descriptor order matches declaration order
    assert_eq!(multi_specs[0].using, "gist");
    assert_eq!(multi_specs[0].extension_dependency, Some("btree_gist"));
    assert_eq!(multi_specs[1].using, "btree");
    assert_eq!(multi_specs[1].extension_dependency, None);

    // CalendarSlot uses pure `&&` overlap → must NOT request btree_gist
    // (stock GiST handles range overlap natively).
    let calendar_specs = <CalendarSlot as Model>::descriptor().exclusion_constraints;
    assert_eq!(calendar_specs.len(), 1);
    assert_eq!(calendar_specs[0].extension_dependency, None);
}
