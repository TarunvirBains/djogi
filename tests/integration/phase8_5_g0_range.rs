// Phase 8.5 G0 + #215 — live `Range<T>` codec and predicate coverage
// end-to-end through Postgres.
//
// # What this file pins
//
// The compile-pass fixture
// (`djogi-macros/tests/compile_pass/g0_range_field.rs`) proves
// the descriptor surface for `Range<T>` columns lowers to the right
// `FieldSqlType::Range { subtype: … }`, and the unit tests in
// `djogi/src/pg_types.rs` exercise the wire codec end-to-end against
// hand-crafted byte buffers. This file closes the loop: it INSERTs
// rows through the typed `Model::create` path and re-reads them
// through `Model::get` against a live Postgres database, exercising
// the `decode_bound`-backed `FromSql` path end-to-end and the
// canonicalisation-sensitive lower-inclusive / upper-exclusive shape
// for the high-value element types covered here:
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
// # Predicate round-trip
//
// #215 adds the SQL-only range operator surface behind
// `explicit_pg_predicate()`. This file keeps the original non-range
// filter/decode regression and adds a focused live predicate test for
// every public range operator spelling.

use djogi::prelude::*;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use time::{OffsetDateTime, PrimitiveDateTime};

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

/// Discrete 64-bit integer range column. Postgres canonicalises
/// `int8range` the same way it canonicalises `int4range`.
#[model(table = "phase8_5_g0_range_i64_rows", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct Phase85G0RangeI64Row {
    pub span64: Range<i64>,
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

/// Continuous timestamp-without-timezone range column. This is the #215
/// `tsrange` surface and deliberately uses `time::PrimitiveDateTime`,
/// separate from Djogi's timezone-aware `DateTime` alias.
#[model(table = "phase8_5_215_range_ts_rows", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct Phase85215RangeTsRow {
    pub local_window: Range<PrimitiveDateTime>,
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

/// Discrete date range column. Postgres canonicalises `daterange` to
/// lower-inclusive / upper-exclusive storage form.
#[model(table = "phase8_5_g0_range_date_rows", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct Phase85G0RangeDateRow {
    pub span_date: Range<time::Date>,
    pub label: String,
}

/// Nullable range column used to pin the present-only predicate surface.
#[model(table = "phase8_5_g0_range_nullable_i64_rows", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct Phase85G0NullableRangeI64Row {
    pub maybe_span64: Option<Range<i64>>,
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

// ── (2b) — Range<PrimitiveDateTime> continuous TSRANGE codec ────────────────

#[djogi::djogi_test(sync_models = [Phase85215RangeTsRow])]
async fn range_ts_inclusive_exclusive_round_trip(mut ctx: djogi::DjogiContext) {
    let lower = PrimitiveDateTime::new(
        time::Date::from_calendar_date(2026, time::Month::January, 1).expect("valid lower date"),
        time::Time::MIDNIGHT,
    );
    let upper = PrimitiveDateTime::new(
        time::Date::from_calendar_date(2026, time::Month::January, 2).expect("valid upper date"),
        time::Time::MIDNIGHT,
    );

    let row = Phase85215RangeTsRow::create(
        &mut ctx,
        Phase85215RangeTsRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            local_window: Range::inclusive_exclusive(lower, upper),
            label: "ts-local-window".into(),
        },
    )
    .await
    .expect("inclusive-exclusive Range<PrimitiveDateTime> must round-trip through tsrange");

    assert_eq!(*row.local_window.lower(), RangeBound::Inclusive(lower));
    assert_eq!(*row.local_window.upper(), RangeBound::Exclusive(upper));

    let fetched = Phase85215RangeTsRow::get(&mut ctx, row.id)
        .await
        .expect("get round-trip on Range<PrimitiveDateTime>");
    assert_eq!(*fetched.local_window.lower(), RangeBound::Inclusive(lower));
    assert_eq!(*fetched.local_window.upper(), RangeBound::Exclusive(upper));
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
// This regression still matters after #215: filtering on a non-range column
// while projecting a `Range<T>` column exercises the live SELECT decode path
// independently from the range predicate operators below.

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

// ── (5) — #215 SQL-only range predicate operators ───────────────────────────

fn sorted_range_labels(rows: Vec<Phase85G0RangeI32Row>) -> Vec<String> {
    let mut labels = rows.into_iter().map(|row| row.label).collect::<Vec<_>>();
    labels.sort();
    labels
}

async fn seed_range_predicate_rows(ctx: &mut djogi::DjogiContext) {
    for (span, label) in [
        (Range::inclusive_exclusive(1_i32, 5_i32), "a"),
        (Range::inclusive_exclusive(5_i32, 10_i32), "b"),
        (Range::inclusive_exclusive(10_i32, 15_i32), "c"),
        (Range::inclusive_exclusive(20_i32, 30_i32), "d"),
    ] {
        Phase85G0RangeI32Row::create(
            ctx,
            Phase85G0RangeI32Row {
                id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
                created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                span,
                label: label.into(),
            },
        )
        .await
        .expect("seed range predicate row");
    }
}

#[djogi::djogi_test(sync_models = [Phase85G0RangeI32Row])]
async fn range_i32_predicate_operators_filter_rows(mut ctx: djogi::DjogiContext) {
    seed_range_predicate_rows(&mut ctx).await;

    let contains = Phase85G0RangeI32Row::objects()
        .filter(|f| f.span().explicit_pg_predicate().contains(3_i32))
        .fetch_all(&mut ctx)
        .await
        .expect("range contains element predicate");
    assert_eq!(sorted_range_labels(contains), ["a"]);

    let contains_range = Phase85G0RangeI32Row::objects()
        .filter(|f| {
            f.span()
                .explicit_pg_predicate()
                .contains_range(Range::inclusive_exclusive(2_i32, 4_i32))
        })
        .fetch_all(&mut ctx)
        .await
        .expect("range contains range predicate");
    assert_eq!(sorted_range_labels(contains_range), ["a"]);

    let contained_by = Phase85G0RangeI32Row::objects()
        .filter(|f| {
            f.span()
                .explicit_pg_predicate()
                .contained_by(Range::inclusive_exclusive(0_i32, 12_i32))
        })
        .fetch_all(&mut ctx)
        .await
        .expect("range contained-by predicate");
    assert_eq!(sorted_range_labels(contained_by), ["a", "b"]);

    let overlaps = Phase85G0RangeI32Row::objects()
        .filter(|f| {
            f.span()
                .explicit_pg_predicate()
                .overlaps(Range::inclusive_exclusive(4_i32, 6_i32))
        })
        .fetch_all(&mut ctx)
        .await
        .expect("range overlaps predicate");
    assert_eq!(sorted_range_labels(overlaps), ["a", "b"]);

    let strictly_left = Phase85G0RangeI32Row::objects()
        .filter(|f| {
            f.span()
                .explicit_pg_predicate()
                .strictly_left_of(Range::inclusive_exclusive(7_i32, 9_i32))
        })
        .fetch_all(&mut ctx)
        .await
        .expect("range strictly-left predicate");
    assert_eq!(sorted_range_labels(strictly_left), ["a"]);

    let strictly_right = Phase85G0RangeI32Row::objects()
        .filter(|f| {
            f.span()
                .explicit_pg_predicate()
                .strictly_right_of(Range::inclusive_exclusive(7_i32, 9_i32))
        })
        .fetch_all(&mut ctx)
        .await
        .expect("range strictly-right predicate");
    assert_eq!(sorted_range_labels(strictly_right), ["c", "d"]);

    let not_extends_right = Phase85G0RangeI32Row::objects()
        .filter(|f| {
            f.span()
                .explicit_pg_predicate()
                .not_extends_right_of(Range::inclusive_exclusive(5_i32, 10_i32))
        })
        .fetch_all(&mut ctx)
        .await
        .expect("range not-extends-right predicate");
    assert_eq!(sorted_range_labels(not_extends_right), ["a", "b"]);

    let not_extends_left = Phase85G0RangeI32Row::objects()
        .filter(|f| {
            f.span()
                .explicit_pg_predicate()
                .not_extends_left_of(Range::inclusive_exclusive(5_i32, 10_i32))
        })
        .fetch_all(&mut ctx)
        .await
        .expect("range not-extends-left predicate");
    assert_eq!(sorted_range_labels(not_extends_left), ["b", "c", "d"]);

    let adjacent = Phase85G0RangeI32Row::objects()
        .filter(|f| {
            f.span()
                .explicit_pg_predicate()
                .adjacent_to(Range::inclusive_exclusive(5_i32, 10_i32))
        })
        .fetch_all(&mut ctx)
        .await
        .expect("range adjacent-to predicate");
    assert_eq!(sorted_range_labels(adjacent), ["a", "c"]);
}

#[djogi::djogi_test(sync_models = [Phase85G0RangeI64Row])]
async fn range_i64_contains_element_filter_rows(mut ctx: djogi::DjogiContext) {
    for (span64, label) in [
        (Range::inclusive_exclusive(1_i64, 5_i64), "a"),
        (Range::inclusive_exclusive(5_i64, 10_i64), "b"),
    ] {
        Phase85G0RangeI64Row::create(
            &mut ctx,
            Phase85G0RangeI64Row {
                id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
                created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                span64,
                label: label.into(),
            },
        )
        .await
        .expect("seed i64 range row");
    }

    let rows = Phase85G0RangeI64Row::objects()
        .filter(|f| f.span64().explicit_pg_predicate().contains(3_i64))
        .fetch_all(&mut ctx)
        .await
        .expect("range<i64> contains element predicate");
    assert_eq!(
        rows.into_iter().map(|row| row.label).collect::<Vec<_>>(),
        ["a"]
    );
}

#[djogi::djogi_test(sync_models = [Phase85G0RangeDecimalRow])]
async fn range_decimal_contains_element_filter_rows(mut ctx: djogi::DjogiContext) {
    for (money, label) in [
        (Range::inclusive_exclusive(dec!(1.50), dec!(9.50)), "a"),
        (Range::inclusive_exclusive(dec!(9.50), dec!(20.00)), "b"),
    ] {
        Phase85G0RangeDecimalRow::create(
            &mut ctx,
            Phase85G0RangeDecimalRow {
                id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
                created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                money,
                label: label.into(),
            },
        )
        .await
        .expect("seed decimal range row");
    }

    let rows = Phase85G0RangeDecimalRow::objects()
        .filter(|f| f.money().explicit_pg_predicate().contains(dec!(2.25)))
        .fetch_all(&mut ctx)
        .await
        .expect("range<decimal> contains element predicate");
    assert_eq!(
        rows.into_iter().map(|row| row.label).collect::<Vec<_>>(),
        ["a"]
    );
}

#[djogi::djogi_test(sync_models = [Phase85G0RangeTstzRow])]
async fn range_tstz_contains_element_filter_rows(mut ctx: djogi::DjogiContext) {
    let lower_a = OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp");
    let upper_a = OffsetDateTime::from_unix_timestamp(1_700_086_400).expect("valid timestamp");
    let lower_b = upper_a;
    let upper_b = OffsetDateTime::from_unix_timestamp(1_700_172_800).expect("valid timestamp");
    for (booking_window, label) in [
        (Range::inclusive_exclusive(lower_a, upper_a), "a"),
        (Range::inclusive_exclusive(lower_b, upper_b), "b"),
    ] {
        Phase85G0RangeTstzRow::create(
            &mut ctx,
            Phase85G0RangeTstzRow {
                id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
                created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                booking_window,
                label: label.into(),
            },
        )
        .await
        .expect("seed tstz range row");
    }

    let probe = OffsetDateTime::from_unix_timestamp(1_700_043_200).expect("valid timestamp");
    let rows = Phase85G0RangeTstzRow::objects()
        .filter(|f| f.booking_window().explicit_pg_predicate().contains(probe))
        .fetch_all(&mut ctx)
        .await
        .expect("range<tstz> contains element predicate");
    assert_eq!(
        rows.into_iter().map(|row| row.label).collect::<Vec<_>>(),
        ["a"]
    );
}

#[djogi::djogi_test(sync_models = [Phase85215RangeTsRow])]
async fn range_ts_contains_element_filter_rows(mut ctx: djogi::DjogiContext) {
    let lower_a = PrimitiveDateTime::new(
        time::Date::from_calendar_date(2026, time::Month::January, 1).expect("valid lower date"),
        time::Time::MIDNIGHT,
    );
    let upper_a = PrimitiveDateTime::new(
        time::Date::from_calendar_date(2026, time::Month::January, 2).expect("valid upper date"),
        time::Time::MIDNIGHT,
    );
    let lower_b = upper_a;
    let upper_b = PrimitiveDateTime::new(
        time::Date::from_calendar_date(2026, time::Month::January, 3).expect("valid upper date"),
        time::Time::MIDNIGHT,
    );
    for (local_window, label) in [
        (Range::inclusive_exclusive(lower_a, upper_a), "a"),
        (Range::inclusive_exclusive(lower_b, upper_b), "b"),
    ] {
        Phase85215RangeTsRow::create(
            &mut ctx,
            Phase85215RangeTsRow {
                id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
                created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                local_window,
                label: label.into(),
            },
        )
        .await
        .expect("seed ts range row");
    }

    let probe = PrimitiveDateTime::new(
        time::Date::from_calendar_date(2026, time::Month::January, 1).expect("valid probe date"),
        time::Time::from_hms(12, 0, 0).expect("valid probe time"),
    );
    let rows = Phase85215RangeTsRow::objects()
        .filter(|f| f.local_window().explicit_pg_predicate().contains(probe))
        .fetch_all(&mut ctx)
        .await
        .expect("range<timestamp> contains element predicate");
    assert_eq!(
        rows.into_iter().map(|row| row.label).collect::<Vec<_>>(),
        ["a"]
    );
}

#[djogi::djogi_test(sync_models = [Phase85G0RangeDateRow])]
async fn range_date_contains_element_filter_rows(mut ctx: djogi::DjogiContext) {
    let jan_1 = time::Date::from_calendar_date(2026, time::Month::January, 1).expect("valid date");
    let jan_5 = time::Date::from_calendar_date(2026, time::Month::January, 5).expect("valid date");
    let jan_10 =
        time::Date::from_calendar_date(2026, time::Month::January, 10).expect("valid date");
    let jan_15 =
        time::Date::from_calendar_date(2026, time::Month::January, 15).expect("valid date");
    for (span_date, label) in [
        (Range::inclusive_exclusive(jan_1, jan_5), "a"),
        (Range::inclusive_exclusive(jan_10, jan_15), "b"),
    ] {
        Phase85G0RangeDateRow::create(
            &mut ctx,
            Phase85G0RangeDateRow {
                id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
                created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                span_date,
                label: label.into(),
            },
        )
        .await
        .expect("seed date range row");
    }

    let probe = time::Date::from_calendar_date(2026, time::Month::January, 3).expect("valid date");
    let rows = Phase85G0RangeDateRow::objects()
        .filter(|f| f.span_date().explicit_pg_predicate().contains(probe))
        .fetch_all(&mut ctx)
        .await
        .expect("range<date> contains element predicate");
    assert_eq!(
        rows.into_iter().map(|row| row.label).collect::<Vec<_>>(),
        ["a"]
    );
}

#[djogi::djogi_test(sync_models = [Phase85G0NullableRangeI64Row])]
async fn nullable_range_i64_present_only_contains_element_filter_rows(
    mut ctx: djogi::DjogiContext,
) {
    for (maybe_span64, label) in [
        (Some(Range::inclusive_exclusive(1_i64, 5_i64)), "a"),
        (Some(Range::inclusive_exclusive(5_i64, 10_i64)), "b"),
        (None, "nil"),
    ] {
        Phase85G0NullableRangeI64Row::create(
            &mut ctx,
            Phase85G0NullableRangeI64Row {
                id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
                created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                maybe_span64,
                label: label.into(),
            },
        )
        .await
        .expect("seed nullable i64 range row");
    }

    let rows = Phase85G0NullableRangeI64Row::objects()
        .filter(|f| f.maybe_span64().some().contains(3_i64))
        .fetch_all(&mut ctx)
        .await
        .expect("nullable range<i64> present-only contains element predicate");
    assert_eq!(
        rows.into_iter().map(|row| row.label).collect::<Vec<_>>(),
        ["a"]
    );
}
