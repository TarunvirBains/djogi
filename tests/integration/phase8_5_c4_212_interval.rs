// Phase 8.5 Cluster 4 (djogi#212) — INTERVAL typed-surface round-trip.
//
// # What this file pins
//
// 1. **Descriptor projection.** A model field typed `djogi::Interval`
//    lowers to `FieldSqlType::Interval` and the migration composer
//    emits `INTERVAL` in the column-type slot of `CREATE TABLE`.
// 2. **Wire-format round-trip.** A row whose `Interval` columns carry
//    mixed (`months`, `days`, `microseconds`) components round-trips
//    end-to-end through `Model::create` → `RETURNING *` → `FromPgRow`
//    decode, preserving every component byte-for-byte.
// 3. **Independent components.** Each constructor variant
//    (`months_only`, `days_only`, `microseconds_only`) round-trips
//    cleanly — a non-zero component in one field does not bleed into
//    the others through the wire codec.
// 4. **Boundary values.** `i32::MIN` / `i32::MAX` on the months and
//    days fields, and `i64::MIN` / `i64::MAX` on the microseconds
//    field, round-trip without overflow on either the bind or the
//    decode side.
//
// # No raw_execute required
//
// The Interval newtype is fully constructable through djogi's typed
// surface, so this test lives under `tests/integration/` (the
// raw-free integration target) rather than `tests/internal/`. The
// pin tests for individual raw_* APIs are independent of this file.

use djogi::Interval;
use djogi::prelude::*;

// ── Test model — one Interval column + one nullable Interval column ──────────

#[model(table = "phase8_5_c4_212_interval_rows", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct Phase85C4212IntervalRow {
    pub duration: Interval,
    pub maybe_duration: Option<Interval>,
    pub label: String,
}

// ── Round-trip tests ─────────────────────────────────────────────────────────

#[djogi::djogi_test(sync_models = [Phase85C4212IntervalRow])]
async fn interval_mixed_components_round_trip(mut ctx: djogi::DjogiContext) {
    // Mixed components: 1 month + 2 days + 3.5 seconds. The Postgres
    // wire format carries the three fields as separate ints, so the
    // round-trip must preserve every one — a buggy codec that
    // conflated `microseconds` with `days * 86_400_000_000` would
    // surface here.
    let row = Phase85C4212IntervalRow::create(
        &mut ctx,
        Phase85C4212IntervalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            duration: Interval {
                months: 1,
                days: 2,
                microseconds: 3_500_000,
            },
            maybe_duration: Some(Interval::months_only(6)),
            label: "djogi#212 mixed".into(),
        },
    )
    .await
    .expect("mixed Interval columns must round-trip through Model::create");

    assert_eq!(row.duration.months, 1);
    assert_eq!(row.duration.days, 2);
    assert_eq!(row.duration.microseconds, 3_500_000);
    assert_eq!(
        row.maybe_duration,
        Some(Interval {
            months: 6,
            days: 0,
            microseconds: 0
        })
    );

    // Re-fetch through Model::get to exercise the full decode path.
    let fetched = Phase85C4212IntervalRow::get(&mut ctx, row.id)
        .await
        .expect("Model::get round-trip");
    assert_eq!(fetched.duration, row.duration);
    assert_eq!(fetched.maybe_duration, row.maybe_duration);
}

#[djogi::djogi_test(sync_models = [Phase85C4212IntervalRow])]
async fn interval_independent_components_round_trip(mut ctx: djogi::DjogiContext) {
    // Three separate rows, each with exactly one component populated.
    // A codec bug that swapped or reordered the wire fields would
    // surface as one of the rows decoding back with the wrong
    // component carrying the value.
    let months_row = Phase85C4212IntervalRow::create(
        &mut ctx,
        Phase85C4212IntervalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            duration: Interval::months_only(13),
            maybe_duration: None,
            label: "months-only".into(),
        },
    )
    .await
    .expect("months-only Interval round-trip");
    assert_eq!(months_row.duration.months, 13);
    assert_eq!(months_row.duration.days, 0);
    assert_eq!(months_row.duration.microseconds, 0);

    let days_row = Phase85C4212IntervalRow::create(
        &mut ctx,
        Phase85C4212IntervalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            duration: Interval::days_only(42),
            maybe_duration: None,
            label: "days-only".into(),
        },
    )
    .await
    .expect("days-only Interval round-trip");
    assert_eq!(days_row.duration.months, 0);
    assert_eq!(days_row.duration.days, 42);
    assert_eq!(days_row.duration.microseconds, 0);

    let us_row = Phase85C4212IntervalRow::create(
        &mut ctx,
        Phase85C4212IntervalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            duration: Interval::microseconds_only(1_500_000),
            maybe_duration: None,
            label: "microseconds-only".into(),
        },
    )
    .await
    .expect("microseconds-only Interval round-trip");
    assert_eq!(us_row.duration.months, 0);
    assert_eq!(us_row.duration.days, 0);
    assert_eq!(us_row.duration.microseconds, 1_500_000);
}

#[djogi::djogi_test(sync_models = [Phase85C4212IntervalRow])]
async fn interval_signed_components_round_trip(mut ctx: djogi::DjogiContext) {
    // Postgres INTERVAL admits negative components (`INTERVAL '-3
    // months -42 days -999_999 microseconds'`). The Rust newtype
    // mirrors this with signed `i32` / `i64` fields; the wire codec
    // must preserve the sign bit on every component.
    let row = Phase85C4212IntervalRow::create(
        &mut ctx,
        Phase85C4212IntervalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            duration: Interval {
                months: -3,
                days: -42,
                microseconds: -999_999,
            },
            maybe_duration: None,
            label: "negative components".into(),
        },
    )
    .await
    .expect("negative-component Interval round-trip");
    assert_eq!(row.duration.months, -3);
    assert_eq!(row.duration.days, -42);
    assert_eq!(row.duration.microseconds, -999_999);
}

#[djogi::djogi_test(sync_models = [Phase85C4212IntervalRow])]
async fn interval_null_round_trips_as_none(mut ctx: djogi::DjogiContext) {
    // `Option<Interval>` column carrying `None` must round-trip as
    // SQL NULL — the postgres-types `Option<T>` ToSql impl handles
    // the IsNull::Yes path; the test pins that the typed surface
    // composes cleanly with the Interval codec.
    let row = Phase85C4212IntervalRow::create(
        &mut ctx,
        Phase85C4212IntervalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            duration: Interval::default(),
            maybe_duration: None,
            label: "null maybe".into(),
        },
    )
    .await
    .expect("nullable Interval column with None must round-trip");
    assert_eq!(row.maybe_duration, None);

    let fetched = Phase85C4212IntervalRow::get(&mut ctx, row.id)
        .await
        .expect("Model::get on nullable Interval");
    assert_eq!(fetched.maybe_duration, None);
}

#[djogi::djogi_test(sync_models = [Phase85C4212IntervalRow])]
async fn interval_boundary_components_round_trip(mut ctx: djogi::DjogiContext) {
    let min_row = Phase85C4212IntervalRow::create(
        &mut ctx,
        Phase85C4212IntervalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            duration: Interval {
                months: i32::MIN,
                days: i32::MIN,
                microseconds: i64::MIN,
            },
            maybe_duration: None,
            label: "boundary min".into(),
        },
    )
    .await
    .expect("minimum Interval components must round-trip");
    assert_eq!(min_row.duration.months, i32::MIN);
    assert_eq!(min_row.duration.days, i32::MIN);
    assert_eq!(min_row.duration.microseconds, i64::MIN);

    let max_row = Phase85C4212IntervalRow::create(
        &mut ctx,
        Phase85C4212IntervalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            duration: Interval {
                months: i32::MAX,
                days: i32::MAX,
                microseconds: i64::MAX,
            },
            maybe_duration: Some(Interval {
                months: i32::MIN,
                days: i32::MAX,
                microseconds: i64::MIN,
            }),
            label: "boundary max".into(),
        },
    )
    .await
    .expect("maximum Interval components must round-trip");
    assert_eq!(max_row.duration.months, i32::MAX);
    assert_eq!(max_row.duration.days, i32::MAX);
    assert_eq!(max_row.duration.microseconds, i64::MAX);
    assert_eq!(
        max_row.maybe_duration,
        Some(Interval {
            months: i32::MIN,
            days: i32::MAX,
            microseconds: i64::MIN,
        })
    );
}
