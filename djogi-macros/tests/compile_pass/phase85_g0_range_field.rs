// Phase 8.5 G0 / #215 — `djogi::Range<T>` field type and predicate surface.
//
// Exercises the macro's parse + lower path for `Range<T>` Postgres columns
// and the #215 SQL-only predicate methods. The EXCLUDE constraint grammar
// (djogi#148) and PG18 temporal DDL (djogi#150) are deliberate future
// sibling lanes that consume range columns but do not alter this typed
// surface.
//
// 1. Bare `Range<i32>` / `Range<i64>` / `Range<Decimal>` / `Range<DateTime>`
//    / `Range<time::PrimitiveDateTime>` / `Range<Date>` from
//    `djogi::prelude::*` lower to
//    `FieldSqlType::Range { subtype: … }` with the matching
//    `RangeSubtypeKind` discriminant.
// 2. Path-form policy: `djogi::Range<…>`, `djogi::types::Range<…>`,
//    `::djogi::Range<…>`, and `::djogi::types::Range<…>` route through
//    the runtime-backed Range lowering. Non-djogi outer wrappers named
//    `Range` are compile-fail fixtures, not accepted structural matches.
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
// * Direct portable `f.col().eq(Range::…)` — `Range<T>` equality is
//   PostgreSQL range canonicalization, not Rust structural equality.
//   SQL-only range operators route through `explicit_pg_predicate()`.
// * `#[model(exclude(…))]` grammar — djogi#148, future sibling lane.
// * `WITHOUT OVERLAPS` / `PERIOD` FK / `NOT ENFORCED` / named `NOT NULL`
//   modifiers — djogi#150, future sibling lane.
// * Postgres `btree_gist` extension migration emission — djogi#148.
//
// Those sibling lanes are tracked separately and are not part of #215.

use djogi::prelude::*;
use rust_decimal::Decimal;
use time::PrimitiveDateTime;

// ── (1) Range columns spanning every #215 subtype ───────────────────────────

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
    /// `Range<PrimitiveDateTime>` → `tsrange`. Timestamp without timezone.
    pub ts: Range<PrimitiveDateTime>,
    /// `Range<Date>` → `daterange`. Calendar-day temporal range — the
    /// typical "validity period" column shape.
    pub d: Range<Date>,
    pub label: String,
}

// ── (2) Runtime-backed Range path policy ────────────────────────────────────

#[model(table = "phase85_g0_range_paths", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct RangePathForms {
    /// Bare `Range<…>` from the prelude.
    pub bare: Range<i32>,
    /// `djogi::Range<…>` form.
    pub via_djogi: djogi::Range<i32>,
    /// `djogi::types::Range<…>` form.
    pub via_types: djogi::types::Range<i32>,
    /// Leading-`::` absolute crate-root form.
    pub via_absolute_djogi: ::djogi::Range<i32>,
    /// Leading-`::` absolute `types` form.
    pub via_absolute_types: ::djogi::types::Range<i32>,
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
    let _: &Range<PrimitiveDateTime> = &all.ts;
    let _: &Range<Date> = &all.d;
    let _: &djogi::Range<i32> = &paths.via_djogi;
    let _: &djogi::types::Range<i32> = &paths.via_types;
    let _: &::djogi::Range<i32> = &paths.via_absolute_djogi;
    let _: &::djogi::types::Range<i32> = &paths.via_absolute_types;
    let _: &Range<i32> = &nullable.required;
    let _: &Option<Range<DateTime>> = &nullable.maybe;
}

// ── (5) {Model}Fields accessor compiles for Range<T> columns ────────────────
//
// The bare accessor only requires `Range<T>` to be a sized type; it does NOT
// require portable equality. SQL-only range operator bounds attach to the
// explicit predicate view exercised below.

fn _check_field_accessors() {
    // Accessor returns `DjogiField<RangeAllSubtypes, Range<i32>>` — we
    // only need to bind it and confirm the type compiles; we don't
    // call any predicate / setter method.
    let _all = RangeAllSubtypes::objects().filter(|f| {
        let _i4 = f.i4();
        let _i8 = f.i8col();
        let _num = f.num();
        let _tstz = f.tstz();
        let _ts = f.ts();
        let _d = f.d();
        // Return an always-true predicate from a portable scalar to
        // satisfy the closure's `Condition` return shape without
        // touching the Range field surface.
        f.label().eq("anything".to_string())
    });
}

// ── (6) #215 SQL-only range predicate surface ──────────────────────────────

fn _check_range_predicates_compile() {
    let probe = Range::inclusive_exclusive(2_i32, 4_i32);
    let envelope = Range::inclusive_exclusive(0_i32, 10_i32);

    let _all = RangeAllSubtypes::objects().filter(|f| {
        f.i4().explicit_pg_predicate().contains(3_i32)
            & f.i4().explicit_pg_predicate().contains_range(probe)
            & f.i4()
                .explicit_pg_predicate()
                .contained_by(envelope)
            & f.i4().explicit_pg_predicate().overlaps(probe)
            & f.i4()
                .explicit_pg_predicate()
                .strictly_left_of(envelope)
            & f.i4()
                .explicit_pg_predicate()
                .strictly_right_of(probe)
            & f.i4()
                .explicit_pg_predicate()
                .not_extends_right_of(envelope)
            & f.i4()
                .explicit_pg_predicate()
                .not_extends_left_of(probe)
            & f.i4().explicit_pg_predicate().adjacent_to(probe)
    });

    let _nullable = RangeNullable::objects().filter(|f| f.maybe().some().overlaps(
        Range::inclusive_exclusive(DateTime::UNIX_EPOCH, DateTime::UNIX_EPOCH),
    ));
}

fn main() {}
