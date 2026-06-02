// djogi#212 — INTERVAL typed-surface round-trip.
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
// 5. **Filter execution.** `QuerySet::filter(|f| f.duration().eq(...))`
//    emits a correctly-typed `$1` bind for `INTERVAL` and returns only
//    the matching row — pins the `FilterValue::Interval` → `push_bind`
//    path at SQL-execution level.
// 6. **Bulk-update execution.** `QuerySet::update(|f| f.duration().set(...))`
//    emits the correct `SET duration = $1` clause, executing through the
//    same `push_bind` path — pins the `UpdateAssignment` → `Interval`
//    bind at SQL-execution level.
// 7. **SQL `=` linearization.** `QuerySet::filter(|f| f.duration().eq(...))`
//    forwards to Postgres `=`, which linearizes months as 30 days and
//    days as 24 hours before comparing. `INTERVAL '1 month'` and
//    `INTERVAL '30 days'` are equal in Postgres SQL even though Rust
//    `PartialEq` says they are not. This test pins the deliberate
//    divergence between Rust structural equality and Postgres SQL `=`.
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
async fn interval_present_not_in_empty_excludes_null_rows(mut ctx: djogi::DjogiContext) {
    Phase85C4212IntervalRow::create(
        &mut ctx,
        Phase85C4212IntervalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            duration: Interval::days_only(1),
            maybe_duration: Some(Interval::months_only(1)),
            label: "present-duration".into(),
        },
    )
    .await
    .expect("create present nullable Interval row");

    Phase85C4212IntervalRow::create(
        &mut ctx,
        Phase85C4212IntervalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            duration: Interval::days_only(2),
            maybe_duration: None,
            label: "null-duration".into(),
        },
    )
    .await
    .expect("create null nullable Interval row");

    let results = Phase85C4212IntervalRow::objects()
        .filter(|f| f.maybe_duration().some().not_in(Vec::<Interval>::new()))
        .fetch_all(&mut ctx)
        .await
        .expect("empty present-only Interval NOT IN must execute");

    assert_eq!(
        results.len(),
        1,
        "empty present-only Interval NOT IN must preserve the IS NOT NULL guard"
    );
    assert_eq!(results[0].label, "present-duration");
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

// ── Runtime filter / bulk-update execution (FIX_BEFORE_BETA-1 / djogi#212) ───
//
// These tests pin the `FilterValue::Interval` → `push_bind` and
// `UpdateAssignment` → `push_bind` paths at SQL-execution level.
// Compile-fixture coverage (`interval_field.rs`) proves the
// surface type-checks; these tests prove it executes correctly against a live
// Postgres INTERVAL column.

#[djogi::djogi_test(sync_models = [Phase85C4212IntervalRow])]
async fn interval_filter_eq_returns_matching_row(mut ctx: djogi::DjogiContext) {
    // Two rows with distinct durations. Only the one matching the filter
    // predicate must come back from `fetch_all`.
    let target_duration = Interval::days_only(7);
    let other_duration = Interval::months_only(3);

    Phase85C4212IntervalRow::create(
        &mut ctx,
        Phase85C4212IntervalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            duration: target_duration,
            maybe_duration: None,
            label: "filter-target".into(),
        },
    )
    .await
    .expect("create filter-target row");

    Phase85C4212IntervalRow::create(
        &mut ctx,
        Phase85C4212IntervalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            duration: other_duration,
            maybe_duration: None,
            label: "filter-other".into(),
        },
    )
    .await
    .expect("create filter-other row");

    let results = Phase85C4212IntervalRow::objects()
        .filter(|f| f.duration().eq(target_duration))
        .fetch_all(&mut ctx)
        .await
        .expect("filter by Interval eq must execute without error");

    assert_eq!(
        results.len(),
        1,
        "filter should return exactly one matching row"
    );
    assert_eq!(results[0].label, "filter-target");
    assert_eq!(results[0].duration, target_duration);
}

#[djogi::djogi_test(sync_models = [Phase85C4212IntervalRow])]
async fn interval_bulk_update_sets_duration(mut ctx: djogi::DjogiContext) {
    // Create a row, bulk-update its `duration` through the typed SET path,
    // then re-fetch to confirm the new value persisted in the DB.
    let initial = Interval::days_only(10);
    let updated = Interval::microseconds_only(500_000);

    let row = Phase85C4212IntervalRow::create(
        &mut ctx,
        Phase85C4212IntervalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            duration: initial,
            maybe_duration: None,
            label: "bulk-update-target".into(),
        },
    )
    .await
    .expect("create row for bulk update");

    let n = Phase85C4212IntervalRow::objects()
        .filter(|f| f.duration().eq(initial))
        .update(|f| f.duration().set(updated))
        .execute(&mut ctx)
        .await
        .expect("bulk update of Interval field must execute without error");

    assert_eq!(n, 1, "exactly one row should be updated");

    let fetched = Phase85C4212IntervalRow::get(&mut ctx, row.id)
        .await
        .expect("re-fetch after bulk update");

    assert_eq!(
        fetched.duration, updated,
        "duration must reflect the bulk-updated value"
    );
}

#[djogi::djogi_test(sync_models = [Phase85C4212IntervalRow])]
async fn interval_bulk_update_increments_and_decrements_duration(mut ctx: djogi::DjogiContext) {
    let row = Phase85C4212IntervalRow::create(
        &mut ctx,
        Phase85C4212IntervalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            duration: Interval::days_only(10),
            maybe_duration: None,
            label: "bulk-update-arithmetic-target".into(),
        },
    )
    .await
    .expect("create row for bulk interval arithmetic update");

    let incremented = Phase85C4212IntervalRow::objects()
        .filter(|f| f.id().eq(row.id))
        .update(|f| f.duration().increment(Interval::days_only(4)))
        .execute(&mut ctx)
        .await
        .expect("interval increment update must execute");
    assert_eq!(incremented, 1, "exactly one row should be incremented");

    let current = Phase85C4212IntervalRow::get(&mut ctx, row.id)
        .await
        .expect("re-fetch after interval increment");
    assert_eq!(
        current.duration,
        Interval::days_only(14),
        "duration must reflect interval increment"
    );

    let decremented = Phase85C4212IntervalRow::objects()
        .filter(|f| f.id().eq(row.id))
        .update(|f| f.duration().decrement(Interval::days_only(2)))
        .execute(&mut ctx)
        .await
        .expect("interval decrement update must execute");
    assert_eq!(decremented, 1, "exactly one row should be decremented");

    let updated = Phase85C4212IntervalRow::get(&mut ctx, row.id)
        .await
        .expect("re-fetch after interval decrement");
    assert_eq!(
        updated.duration,
        Interval::days_only(12),
        "duration must reflect interval decrement"
    );
}

#[djogi::djogi_test(sync_models = [Phase85C4212IntervalRow])]
async fn interval_sql_eq_linearizes_months_and_days(mut ctx: djogi::DjogiContext) {
    // This test pins the deliberate divergence between Rust structural
    // `PartialEq` on `Interval` and Postgres SQL `=` on INTERVAL columns.
    //
    // Postgres linearizes INTERVAL before comparing: months are treated as
    // 30 days, days as 24 hours (86,400,000,000 microseconds). As a result,
    // `INTERVAL '1 month' = INTERVAL '30 days'` is true in Postgres SQL, while
    // `Interval::months_only(1) == Interval::days_only(30)` is false in Rust.
    //
    // `QuerySet::filter(|f| f.duration().eq(...))` forwards to Postgres `=`
    // and therefore follows Postgres linearization semantics. This test
    // exercises both directions of that linearization and confirms the
    // Rust-side structural inequality to make the divergence test-visible.

    // Row A: stored as "1 month" in the months component.
    Phase85C4212IntervalRow::create(
        &mut ctx,
        Phase85C4212IntervalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            duration: Interval::months_only(1),
            maybe_duration: None,
            label: "one-month-row".into(),
        },
    )
    .await
    .expect("create one-month-row");

    // Row B: stored as "30 days" in the days component.
    Phase85C4212IntervalRow::create(
        &mut ctx,
        Phase85C4212IntervalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            duration: Interval::days_only(30),
            maybe_duration: None,
            label: "thirty-days-row".into(),
        },
    )
    .await
    .expect("create thirty-days-row");

    // Row C: control row with a clearly distinct duration (500 ms).
    // Its presence ensures the queries below would fail if they returned
    // "everything" instead of the linearization-matched rows only.
    Phase85C4212IntervalRow::create(
        &mut ctx,
        Phase85C4212IntervalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            duration: Interval::microseconds_only(500_000),
            maybe_duration: None,
            label: "control-row".into(),
        },
    )
    .await
    .expect("create control-row");

    // Query 1: filter by Interval::months_only(1).
    // Postgres SQL `=` linearizes 1 month → 30 days, so both "one-month-row"
    // and "thirty-days-row" must match.
    let results_by_month = Phase85C4212IntervalRow::objects()
        .filter(|f| f.duration().eq(Interval::months_only(1)))
        .fetch_all(&mut ctx)
        .await
        .expect("filter by months_only(1) must execute without error");

    let labels_by_month: std::collections::BTreeSet<String> =
        results_by_month.into_iter().map(|r| r.label).collect();
    assert_eq!(
        labels_by_month,
        ["one-month-row", "thirty-days-row"]
            .iter()
            .map(|s| s.to_string())
            .collect::<std::collections::BTreeSet<_>>(),
        "filter by months_only(1) must match both the months row and the days row \
         via Postgres SQL = linearization"
    );

    // Query 2: filter by Interval::days_only(30).
    // Symmetric: 30 days linearizes identically, so the same two rows match.
    let results_by_days = Phase85C4212IntervalRow::objects()
        .filter(|f| f.duration().eq(Interval::days_only(30)))
        .fetch_all(&mut ctx)
        .await
        .expect("filter by days_only(30) must execute without error");

    let labels_by_days: std::collections::BTreeSet<String> =
        results_by_days.into_iter().map(|r| r.label).collect();
    assert_eq!(
        labels_by_days,
        ["one-month-row", "thirty-days-row"]
            .iter()
            .map(|s| s.to_string())
            .collect::<std::collections::BTreeSet<_>>(),
        "filter by days_only(30) must match the same two rows as months_only(1) \
         via symmetric Postgres SQL = linearization"
    );

    // Query 3: filter by Interval::days_only(31).
    // 31 days does not linearize to 30 days, so neither row matches.
    let results_thirty_one = Phase85C4212IntervalRow::objects()
        .filter(|f| f.duration().eq(Interval::days_only(31)))
        .fetch_all(&mut ctx)
        .await
        .expect("filter by days_only(31) must execute without error");

    assert_eq!(
        results_thirty_one.len(),
        0,
        "filter by days_only(31) must return zero rows — 31 days does not \
         linearize to 30 days"
    );

    // Rust-side sanity: structural PartialEq deliberately diverges from
    // Postgres SQL =. This assert documents that the divergence is intentional
    // and catches any future accidental unification.
    assert_ne!(
        Interval::months_only(1),
        Interval::days_only(30),
        "Rust structural PartialEq must remain false for months_only(1) vs \
         days_only(30) — the divergence from Postgres SQL = is intentional"
    );
}
