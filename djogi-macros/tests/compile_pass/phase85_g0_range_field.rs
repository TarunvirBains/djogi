// Phase 8.5 G0 (djogi#148 + #150 substrate) — `djogi::Range<T>` field type.
//
// Exercises the macro's parse + lower path for the new `Range<T>`
// typed Postgres range-column substrate. G0 ships only the substrate
// — the EXCLUDE constraint grammar (djogi#148) and PG18 temporal DDL
// (djogi#150) are deliberate future sibling lanes that consume range
// columns but do not alter their typed surface.
//
// 1. Bare `Range<i32>` / `Range<i64>` / `Range<Decimal>` / `Range<DateTime>`
//    / `Range<Date>` from `djogi::prelude::*` lower to
//    `FieldSqlType::Range { subtype: … }` with the matching
//    `RangeSubtypeKind` discriminant.
// 2. Path-form generality: `djogi::Range<…>` and `djogi::types::Range<…>`
//    forms route through the structural last-segment detection without
//    string explosion of every element-type spelling.
// 3. Nullable `Option<Range<…>>` composes cleanly with the standard
//    `Option<T>` wrapper.
// 4. The `{Model}Fields` accessor compiles for `Range<T>` columns and
//    returns a `DjogiField<Model, Range<T>>` value — the bare accessor
//    requires no extra traits on `Range<T>`.
//
// `no_default` because we want the framework to inject `id` /
// `created_at` / `updated_at` without trying to assemble a `Default`
// for the test row.
//
// What this file deliberately does NOT exercise:
//
// * `f.col().eq(Range::…)` / `f.col().set(Range::…)` — those require
//   `Range<T>: IntoFilterValue` / `DjogiPortableEq`, which is operator-
//   surface work scoped to djogi#148 (EXCLUDE / `&&` / `@>` / `<@` /
//   eight-operator family) rather than the G0 substrate.
// * `#[model(exclude(…))]` grammar — djogi#148, future sibling lane.
// * `WITHOUT OVERLAPS` / `PERIOD` FK / `NOT ENFORCED` / named `NOT NULL`
//   modifiers — djogi#150, future sibling lane.
// * Postgres `btree_gist` extension migration emission — djogi#148.
//
// All four are tracked separately and not part of the G0 substrate scope.

use djogi::prelude::*;
use rust_decimal::Decimal;

// ── (1) Range columns spanning every G0 subtype ──────────────────────────────

#[model(table = "phase85_g0_range_all_subtypes", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct RangeAllSubtypes {
    /// `Range<i32>` → `int4range`. The canonical discrete-integer range.
    pub i4: Range<i32>,
    /// `Range<i64>` → `int8range`.
    pub i8col: Range<i64>,
    /// `Range<Decimal>` → `numrange`. Arbitrary-precision continuous range.
    pub num: Range<Decimal>,
    /// `Range<DateTime>` → `tstzrange`. Timezone-aware temporal range —
    /// the typical "booking window" column shape.
    pub tstz: Range<DateTime>,
    /// `Range<Date>` → `daterange`. Calendar-day temporal range — the
    /// typical "validity period" column shape.
    pub d: Range<Date>,
    pub label: String,
}

// ── (2) Path-form generality through structural last-segment detection ──────

#[model(table = "phase85_g0_range_paths", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct RangePathForms {
    /// Bare `Range<…>` from the prelude — handled by structural
    /// last-segment match.
    pub bare: Range<i32>,
    /// `djogi::Range<…>` form — same structural detection.
    pub via_djogi: djogi::Range<i32>,
    /// `djogi::types::Range<…>` form — also routed through
    /// structural detection. Mirrors the path-form generality already
    /// established for `Jsonb<T>`.
    pub via_types: djogi::types::Range<i32>,
}

// ── (3) Nullable Range columns ──────────────────────────────────────────────

#[model(table = "phase85_g0_range_nullable", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct RangeNullable {
    pub required: Range<i32>,
    /// `Option<Range<…>>` composes with the standard `unwrap_option`
    /// pipeline.
    pub maybe: Option<Range<DateTime>>,
}

// ── (4) Field-type round-trip check at the type level ───────────────────────

fn _check_field_types(all: &RangeAllSubtypes, paths: &RangePathForms, nullable: &RangeNullable) {
    let _: &Range<i32> = &all.i4;
    let _: &Range<i64> = &all.i8col;
    let _: &Range<Decimal> = &all.num;
    let _: &Range<DateTime> = &all.tstz;
    let _: &Range<Date> = &all.d;
    let _: &djogi::Range<i32> = &paths.via_djogi;
    let _: &djogi::types::Range<i32> = &paths.via_types;
    let _: &Range<i32> = &nullable.required;
    let _: &Option<Range<DateTime>> = &nullable.maybe;
}

// ── (5) {Model}Fields accessor compiles for Range<T> columns ────────────────
//
// The bare accessor only requires `Range<T>` to be a sized type; it
// does NOT require `IntoFilterValue` / `DjogiPortableEq` / similar
// operator-surface traits. Those bounds attach to specific method
// calls (`.eq`, `.set`, `.gt`, …), which G0 deliberately does not
// exercise — they are part of the djogi#148 / djogi#150 operator
// surfaces that will plug in later.

fn _check_field_accessors() {
    // Accessor returns `DjogiField<RangeAllSubtypes, Range<i32>>` — we
    // only need to bind it and confirm the type compiles; we don't
    // call any predicate / setter method.
    let _all = RangeAllSubtypes::objects().filter(|f| {
        let _i4 = f.i4();
        let _i8 = f.i8col();
        let _num = f.num();
        let _tstz = f.tstz();
        let _d = f.d();
        // Return an always-true predicate from a portable scalar to
        // satisfy the closure's `Condition` return shape without
        // touching the Range field surface.
        f.label().eq("anything".to_string())
    });
}

fn main() {}
