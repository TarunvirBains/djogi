// Phase 8.5 G0 (djogi#148 + djogi#150 substrate) — live `Range<T>`
// codec coverage end-to-end through Postgres.
//
// # What this file pins
//
// The compile-pass fixture
// (`djogi-macros/tests/compile_pass/phase85_g0_range_field.rs`) proves
// the descriptor surface for `Range<T>` columns lowers to the right
// `FieldSqlType::Range { subtype: … }`, and the unit tests in
// `djogi/src/pg_types.rs` exercise the wire codec end-to-end against
// hand-crafted byte buffers. This file closes the loop: it INSERTs
// rows through the typed `Model::create` path and re-reads them
// through `Model::get` against a live Postgres database, exercising
// the `decode_bound`-backed `FromSql` path end-to-end and the
// canonicalisation-sensitive lower-inclusive / upper-exclusive shape
// for the three high-value element types G0 ships:
//
// 1. **`Range<i32>` discrete canonicalisation.** Postgres canonicalises
//    every `int4range` write to lower-inclusive / upper-exclusive
//    storage form: `[1, 9]` and `[1, 10)` round-trip as the same SQL
//    range. The Rust round-trip preserves whatever shape Postgres
//    returned (`inclusive_exclusive` after canonicalisation).
// 2. **`Range<DateTime>` continuous TIMESTAMPTZ binary path.** The
//    `tstzrange` codec routes finite endpoints through
//    `time::OffsetDateTime`'s 8-byte big-endian wire format; no
//    canonicalisation, the bound shape round-trips as written.
// 3. **`Range<Decimal>` NUMERIC binary path.** The `numrange` codec
//    routes finite endpoints through `rust_decimal::Decimal`'s
//    Postgres `ToSql` / `FromSql` impl — the same wire path that
//    feeds the `decode_bound` chain for arbitrary-precision values.
//
// Every column is also exercised in three bound shapes:
//
// * **Empty range** — `Range::empty()`. Postgres marks the wire byte
//   with the `RANGE_EMPTY` flag and carries no bound bytes; the
//   decoder reconstructs the empty range without consulting `T`.
// * **Unbounded on one side** — `(Inclusive(x), Unbounded)` /
//   `(Unbounded, Exclusive(y))`. The decoder must set the matching
//   `RANGE_LB_INF` / `RANGE_UB_INF` flag and skip the bound bytes.
// * **Lower-inclusive / upper-exclusive** — the canonical discrete
//   shape AND the typical adopter "booking window" shape. The
//   decoder reconstructs both flags from the wire bits.
//
// # Predicate round-trip — out of scope for G0
//
// G0 ships the substrate only; `Range<T>: IntoFilterValue` /
// `Range<T>: DjogiPortableEq` (the operator-surface traits required
// by `f.col().eq(Range::…)` / `f.col().contains(…)`) are djogi#148 +
// djogi#150 follow-on lanes. To satisfy the "at least one typed
// filter or SQL predicate round-trip" obligation without dragging in
// new public-surface work, this file filters on a non-range column
// (`label`) while keeping the `Range<T>` column in the projection;
// that path still exercises the `Range<T>` FromSql decode for the
// rows that come back, which is the live path G0 ships.

use djogi::prelude::*;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use time::OffsetDateTime;

// ── Test models ──────────────────────────────────────────────────────────────

/// Discrete-integer range column. Postgres canonicalises every
/// `int4range` write to lower-inclusive / upper-exclusive storage
/// form; the Rust round-trip reflects the canonicalised shape.
#[model(table = "phase8_5_g0_range_i32_rows", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct Phase85G0RangeI32Row {
    pub span: Range<i32>,
    pub label: String,
}

/// Continuous timezone-aware temporal range column. Postgres does NOT
/// canonicalise `tstzrange` storage; the bound shape round-trips as
/// written.
#[model(table = "phase8_5_g0_range_tstz_rows", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct Phase85G0RangeTstzRow {
    pub booking_window: Range<DateTime>,
    pub label: String,
}

/// Continuous arbitrary-precision numeric range column. The
/// `decode_bound` chain hands each finite endpoint to
/// `rust_decimal::Decimal::from_sql`; the round-trip preserves the
/// rust_decimal coefficient + scale verbatim.
#[model(table = "phase8_5_g0_range_decimal_rows", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct Phase85G0RangeDecimalRow {
    pub money: Range<Decimal>,
    pub label: String,
}

// ── (1) — Range<i32> canonicalisation + bound-shape coverage ─────────────────

#[djogi::djogi_test(sync_models = [Phase85G0RangeI32Row])]
async fn range_i32_inclusive_exclusive_round_trip(mut ctx: djogi::DjogiContext) {
    // Canonical discrete shape. Postgres stores it as written; the
    // round-trip preserves `Inclusive(1) / Exclusive(10)` byte for
    // byte.
    let row = Phase85G0RangeI32Row::create(
        &mut ctx,
        Phase85G0RangeI32Row {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            span: Range::inclusive_exclusive(1_i32, 10_i32),
            label: "i32-canonical".into(),
        },
    )
    .await
    .expect("canonical [1,10) Range<i32> must round-trip");

    assert_eq!(*row.span.lower(), RangeBound::Inclusive(1));
    assert_eq!(*row.span.upper(), RangeBound::Exclusive(10));

    let fetched = Phase85G0RangeI32Row::get(&mut ctx, row.id)
        .await
        .expect("get round-trip");
    assert_eq!(*fetched.span.lower(), RangeBound::Inclusive(1));
    assert_eq!(*fetched.span.upper(), RangeBound::Exclusive(10));
}

#[djogi::djogi_test(sync_models = [Phase85G0RangeI32Row])]
async fn range_i32_inclusive_canonicalises_to_inclusive_exclusive(mut ctx: djogi::DjogiContext) {
    // Postgres canonicalises `int4range '[1,9]'` to the equivalent
    // canonical form `[1,10)` at storage time. The Rust round-trip
    // reflects what Postgres stored: the upper bound comes back as
    // `Exclusive(10)`, not `Inclusive(9)`. The lower bound stays
    // `Inclusive(1)` because that side is already in canonical form.
    let row = Phase85G0RangeI32Row::create(
        &mut ctx,
        Phase85G0RangeI32Row {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            span: Range::inclusive(1_i32, 9_i32),
            label: "i32-canonicalises".into(),
        },
    )
    .await
    .expect("`[1,9]` Range<i32> must round-trip (canonicalised to `[1,10)` at storage)");

    assert_eq!(
        *row.span.lower(),
        RangeBound::Inclusive(1),
        "lower bound is already in canonical inclusive form; should round-trip unchanged"
    );
    assert_eq!(
        *row.span.upper(),
        RangeBound::Exclusive(10),
        "upper inclusive 9 must canonicalise to exclusive 10 on the discrete int4range type"
    );
}

#[djogi::djogi_test(sync_models = [Phase85G0RangeI32Row])]
async fn range_i32_empty_round_trip(mut ctx: djogi::DjogiContext) {
    // The empty range encodes as a single `RANGE_EMPTY` flag byte and
    // carries no bound bytes; the decoder reconstructs
    // `Range::empty()` without consulting the element type.
    let row = Phase85G0RangeI32Row::create(
        &mut ctx,
        Phase85G0RangeI32Row {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            span: Range::empty(),
            label: "i32-empty".into(),
        },
    )
    .await
    .expect("empty Range<i32> must round-trip");

    assert!(row.span.is_empty());
    let fetched = Phase85G0RangeI32Row::get(&mut ctx, row.id)
        .await
        .expect("get round-trip");
    assert!(fetched.span.is_empty());
}

#[djogi::djogi_test(sync_models = [Phase85G0RangeI32Row])]
async fn range_i32_unbounded_lower_round_trip(mut ctx: djogi::DjogiContext) {
    // `(-inf, 5)`: the wire format sets `RANGE_LB_INF` and writes only
    // the upper bound bytes. The decoder must skip the missing lower
    // bound and reconstruct `RangeBound::Unbounded`.
    let row = Phase85G0RangeI32Row::create(
        &mut ctx,
        Phase85G0RangeI32Row {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            span: Range::new(RangeBound::Unbounded, RangeBound::Exclusive(5)),
            label: "i32-unbounded-lower".into(),
        },
    )
    .await
    .expect("(-inf, 5) Range<i32> must round-trip");

    assert_eq!(*row.span.lower(), RangeBound::Unbounded);
    assert_eq!(*row.span.upper(), RangeBound::Exclusive(5));
}

#[djogi::djogi_test(sync_models = [Phase85G0RangeI32Row])]
async fn range_i32_unbounded_upper_round_trip(mut ctx: djogi::DjogiContext) {
    // `[5, +inf)`: sets `RANGE_UB_INF` and writes only the lower
    // bound bytes.
    let row = Phase85G0RangeI32Row::create(
        &mut ctx,
        Phase85G0RangeI32Row {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            span: Range::new(RangeBound::Inclusive(5), RangeBound::Unbounded),
            label: "i32-unbounded-upper".into(),
        },
    )
    .await
    .expect("[5, +inf) Range<i32> must round-trip");

    assert_eq!(*row.span.lower(), RangeBound::Inclusive(5));
    assert_eq!(*row.span.upper(), RangeBound::Unbounded);
}

// ── (2) — Range<DateTime> continuous TIMESTAMPTZ codec ───────────────────────

#[djogi::djogi_test(sync_models = [Phase85G0RangeTstzRow])]
async fn range_tstz_inclusive_exclusive_round_trip(mut ctx: djogi::DjogiContext) {
    // Booking-window shape: lower-inclusive / upper-exclusive. The
    // `tstzrange` codec is continuous; Postgres does NOT canonicalise
    // bound shapes, so what you write is what you read back.
    let lower = OffsetDateTime::from_unix_timestamp(1_700_000_000)
        .expect("valid UNIX timestamp must produce a valid OffsetDateTime");
    let upper = OffsetDateTime::from_unix_timestamp(1_700_086_400)
        .expect("valid UNIX timestamp must produce a valid OffsetDateTime");

    let row = Phase85G0RangeTstzRow::create(
        &mut ctx,
        Phase85G0RangeTstzRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            booking_window: Range::inclusive_exclusive(lower, upper),
            label: "tstz-booking-window".into(),
        },
    )
    .await
    .expect("inclusive-exclusive Range<DateTime> must round-trip");

    assert_eq!(*row.booking_window.lower(), RangeBound::Inclusive(lower));
    assert_eq!(*row.booking_window.upper(), RangeBound::Exclusive(upper));

    let fetched = Phase85G0RangeTstzRow::get(&mut ctx, row.id)
        .await
        .expect("get round-trip on Range<DateTime>");
    assert_eq!(
        *fetched.booking_window.lower(),
        RangeBound::Inclusive(lower)
    );
    assert_eq!(
        *fetched.booking_window.upper(),
        RangeBound::Exclusive(upper)
    );
}

#[djogi::djogi_test(sync_models = [Phase85G0RangeTstzRow])]
async fn range_tstz_empty_round_trip(mut ctx: djogi::DjogiContext) {
    let row = Phase85G0RangeTstzRow::create(
        &mut ctx,
        Phase85G0RangeTstzRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            booking_window: Range::empty(),
            label: "tstz-empty".into(),
        },
    )
    .await
    .expect("empty Range<DateTime> must round-trip");

    assert!(row.booking_window.is_empty());
}

#[djogi::djogi_test(sync_models = [Phase85G0RangeTstzRow])]
async fn range_tstz_unbounded_lower_round_trip(mut ctx: djogi::DjogiContext) {
    // "Anything before <upper>" — the historical shape for "starts
    // at the dawn of time" booking windows. Lower side is
    // `RangeBound::Unbounded`, upper side carries the bound bytes.
    let upper = OffsetDateTime::from_unix_timestamp(1_700_000_000)
        .expect("valid UNIX timestamp must produce a valid OffsetDateTime");

    let row = Phase85G0RangeTstzRow::create(
        &mut ctx,
        Phase85G0RangeTstzRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            booking_window: Range::new(RangeBound::Unbounded, RangeBound::Exclusive(upper)),
            label: "tstz-unbounded-lower".into(),
        },
    )
    .await
    .expect("(-inf, upper) Range<DateTime> must round-trip");

    assert_eq!(*row.booking_window.lower(), RangeBound::Unbounded);
    assert_eq!(*row.booking_window.upper(), RangeBound::Exclusive(upper));
}

// ── (3) — Range<Decimal> continuous NUMERIC codec ────────────────────────────

#[djogi::djogi_test(sync_models = [Phase85G0RangeDecimalRow])]
async fn range_decimal_inclusive_exclusive_round_trip(mut ctx: djogi::DjogiContext) {
    // The `numrange` codec hands each finite endpoint through the
    // `decode_bound` chain to `rust_decimal::Decimal::from_sql`; the
    // round-trip must preserve coefficient + scale verbatim for a
    // typical currency value with mid-scale precision.
    let row = Phase85G0RangeDecimalRow::create(
        &mut ctx,
        Phase85G0RangeDecimalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            money: Range::inclusive_exclusive(dec!(1.50), dec!(99.99)),
            label: "decimal-canonical".into(),
        },
    )
    .await
    .expect("[1.50, 99.99) Range<Decimal> must round-trip");

    assert_eq!(*row.money.lower(), RangeBound::Inclusive(dec!(1.50)));
    assert_eq!(*row.money.upper(), RangeBound::Exclusive(dec!(99.99)));

    let fetched = Phase85G0RangeDecimalRow::get(&mut ctx, row.id)
        .await
        .expect("get round-trip on Range<Decimal>");
    assert_eq!(*fetched.money.lower(), RangeBound::Inclusive(dec!(1.50)));
    assert_eq!(*fetched.money.upper(), RangeBound::Exclusive(dec!(99.99)));
}

#[djogi::djogi_test(sync_models = [Phase85G0RangeDecimalRow])]
async fn range_decimal_high_precision_round_trip(mut ctx: djogi::DjogiContext) {
    // Stretch the `decode_bound` Decimal path with values carrying
    // close to rust_decimal's maximum scale (28). A buggy NUMERIC
    // codec wired through the range bound length prefix would
    // truncate scale or drop trailing digits; this test pins the
    // full-precision round-trip against the wire format.
    //
    // The lower endpoint exercises maximum scale (28 fractional
    // digits, coefficient 1). The upper endpoint exercises a mid-scale
    // value with both significant integer and significant fractional
    // digits — collectively the two stress the `decode_bound` chain on
    // multiple coefficient widths.
    let lower = dec!(0.0000000000000000000000000001); // scale 28, coefficient 1
    let upper = dec!(123456789012345.6789012345678); // 15 integer + 13 fractional = 28 sig digits
    let row = Phase85G0RangeDecimalRow::create(
        &mut ctx,
        Phase85G0RangeDecimalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            money: Range::inclusive_exclusive(lower, upper),
            label: "decimal-precision".into(),
        },
    )
    .await
    .expect("high-precision Range<Decimal> must round-trip");

    assert_eq!(*row.money.lower(), RangeBound::Inclusive(lower));
    assert_eq!(*row.money.upper(), RangeBound::Exclusive(upper));
}

#[djogi::djogi_test(sync_models = [Phase85G0RangeDecimalRow])]
async fn range_decimal_empty_round_trip(mut ctx: djogi::DjogiContext) {
    let row = Phase85G0RangeDecimalRow::create(
        &mut ctx,
        Phase85G0RangeDecimalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            money: Range::empty(),
            label: "decimal-empty".into(),
        },
    )
    .await
    .expect("empty Range<Decimal> must round-trip");

    assert!(row.money.is_empty());
}

#[djogi::djogi_test(sync_models = [Phase85G0RangeDecimalRow])]
async fn range_decimal_unbounded_upper_round_trip(mut ctx: djogi::DjogiContext) {
    // "Anything at or above <lower>" — typical "open-ended price floor"
    // shape. Lower side carries the bound bytes; upper side is
    // `RangeBound::Unbounded`.
    let row = Phase85G0RangeDecimalRow::create(
        &mut ctx,
        Phase85G0RangeDecimalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            money: Range::new(RangeBound::Inclusive(dec!(100.00)), RangeBound::Unbounded),
            label: "decimal-unbounded-upper".into(),
        },
    )
    .await
    .expect("[100.00, +inf) Range<Decimal> must round-trip");

    assert_eq!(*row.money.lower(), RangeBound::Inclusive(dec!(100.00)));
    assert_eq!(*row.money.upper(), RangeBound::Unbounded);
}

// ── (4) — SQL predicate round-trip on a Range<T> column ──────────────────────
//
// G0 does not ship `Range<T>: IntoFilterValue` (operator-surface work
// belongs to djogi#148 / djogi#150). To still exercise a typed-filter
// round-trip that decodes a `Range<T>` column from the projection,
// filter on a non-range column (`label`) and assert that the
// `Range<T>` field on the returned row decoded correctly through the
// live SELECT path.

#[djogi::djogi_test(sync_models = [Phase85G0RangeI32Row])]
async fn range_i32_filter_returns_decoded_range_column(mut ctx: djogi::DjogiContext) {
    Phase85G0RangeI32Row::create(
        &mut ctx,
        Phase85G0RangeI32Row {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            span: Range::inclusive_exclusive(2_i32, 8_i32),
            label: "filter-target".into(),
        },
    )
    .await
    .expect("create filter-target row");

    Phase85G0RangeI32Row::create(
        &mut ctx,
        Phase85G0RangeI32Row {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            span: Range::empty(),
            label: "filter-other".into(),
        },
    )
    .await
    .expect("create filter-other row");

    let rows = Phase85G0RangeI32Row::objects()
        .filter(|f| f.label().eq("filter-target".to_string()))
        .fetch_all(&mut ctx)
        .await
        .expect("filter by label must execute and decode the Range<i32> column");

    assert_eq!(
        rows.len(),
        1,
        "filter must return exactly one row matching the label"
    );
    assert_eq!(rows[0].label, "filter-target");
    assert_eq!(*rows[0].span.lower(), RangeBound::Inclusive(2));
    assert_eq!(*rows[0].span.upper(), RangeBound::Exclusive(8));
}
