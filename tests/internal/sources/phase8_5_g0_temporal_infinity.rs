// Phase 8.5 G0 — Temporal representability CHECK rejects PostgreSQL
// DATE / TIMESTAMPTZ special values across every projected surface.
//
// # What this file pins
//
// 1. **Scalar Date CHECK rejects `+infinity` / `-infinity`.**
//    A `time::Date` column projects
//    `pg_catalog.isfinite(<col>) AND <col> <= DATE '9999-12-31'`. The
//    leading `isfinite(<col>)` clause rejects PostgreSQL's two
//    non-finite DATE special values that `time::Date` cannot decode
//    at all. Without the guard a raw `INSERT … DATE '-infinity'` lands
//    successfully (because `'-infinity'::date <= DATE '9999-12-31'`
//    is TRUE) and poisons the next typed `time::Date::from_sql`
//    decode with `DjogiError::Decode`.
// 2. **Scalar Timestamptz CHECK rejects `+infinity` / `-infinity`.**
//    Same shape on the `time::OffsetDateTime` column. The CHECK
//    expression carries the UTC-explicit `+00` upper-bound literal
//    plus the `isfinite(<col>)` guard.
// 3. **DATERANGE endpoint CHECK rejects special-value endpoints.** The
//    `Range<time::Date>` column projects the same `date_range_expr`
//    through `range_endpoint_checks(<col>, date_range_expr)` so both
//    `lower(<col>)` and `upper(<col>)` carry the finite guard inside
//    their `IS NULL OR (...)` pass-through wrapper. Empty / unbounded
//    / NULL ranges still pass via the short-circuit on `IS NULL`.
// 4. **TSTZRANGE endpoint CHECK rejects special-value endpoints.**
//    Same shape on the `Range<time::OffsetDateTime>` column.
// 5. **Finite values still round-trip.** Regular `time::Date` /
//    `time::OffsetDateTime` values continue to pass the CHECK on all
//    four surfaces — the finite-value guard does not regress the
//    existing accept path.
// 6. **Unbounded ranges still round-trip.** A `Range::new(Unbounded,
//    Inclusive(date))` value carries `lower(range) = NULL` and the
//    `<endpoint> IS NULL OR (...)` short-circuit passes the row
//    through without ever evaluating the inner `isfinite` clause.
// 7. **DATE[] elements CHECK rejects `+infinity` / `-infinity`.**
//    A `Vec<time::Date>` column projects
//    `<col> IS NULL OR djogi.__djogi_date_array_is_finite_v1(<col>)`.
//    The helper applies `pg_catalog.isfinite` per element via
//    `bool_and(isfinite(elem) AND elem <= ...)` over `unnest`. The
//    previous upper-bound-only `ALL(col)` strategy admitted
//    `-infinity` because `-infinity < upper_bound` (making the `>=`
//    comparison TRUE), so the helper replaces it entirely.
// 8. **TIMESTAMPTZ[] elements CHECK rejects `+infinity` / `-infinity`.**
//    Same via `djogi.__djogi_tstz_array_is_finite_v1`. Empty arrays
//    and NULL arrays continue to pass.
//
// # Why a tests/internal target
//
// `time::Date`, `time::OffsetDateTime`, and their `Vec<_>` wrappers
// cannot construct `+infinity` or `-infinity` (none of these types
// carry a non-finite state), so the only way to land a Postgres DATE /
// TIMESTAMPTZ special value is to hand-craft
// `INSERT … DATE '-infinity'` / `ARRAY['-infinity'::date]` via
// `raw_execute`. Hand-crafted SQL belongs under `tests/internal/`;
// integration tests stay raw-free per CLAUDE.md.
//
// # Spec anchors
//
// - `docs/spec/decisions.md` — "Type-derived CHECK projection"
//   (djogi#187) + "`Range<T>` typed substrate (djogi#215, Phase 8.5 G0)".
// - `djogi/src/migrate/projection.rs::date_range_expr` /
//   `timestamptz_range_expr` — scalar representability predicates.
// - `djogi/src/migrate/projection.rs::date_array_is_finite_check` /
//   `tstz_array_is_finite_check` — array representability predicates.
// - `djogi/src/migrate/projection.rs::range_endpoint_checks` — range
//   adapter for finite-endpoint checks.
// - `djogi/src/migrate/compose.rs::DATE_ARRAY_HELPER_PRELUDE` /
//   `TSTZ_ARRAY_HELPER_PRELUDE` — SQL helper function bodies.

use djogi::prelude::*;

// ── Test models ──────────────────────────────────────────────────────────────
//
// Field names are chosen to avoid PostgreSQL reserved keywords. `when` and
// `window` are reserved (the latter is the WINDOW function clause keyword)
// and the macro rejects them at compile time. `event_on`, `recorded_at`,
// `validity`, and `booking` are non-reserved.

/// Scalar `time::Date` column — exercises the inline `date_range_expr`
/// CHECK directly on the column value.
#[model(table = "phase8_5_g0_temporal_inf_date_scalar", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct TemporalInfDateScalarRow {
    pub event_on: ::time::Date,
    pub label: String,
}

/// Scalar `time::OffsetDateTime` column — exercises the inline
/// `timestamptz_range_expr` CHECK directly on the column value.
#[model(table = "phase8_5_g0_temporal_inf_tstz_scalar", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct TemporalInfTstzScalarRow {
    pub recorded_at: ::time::OffsetDateTime,
    pub label: String,
}

/// `Range<time::Date>` column — exercises the DATERANGE endpoint
/// CHECKs. The descriptor lowers to `daterange`, and the projection
/// emits the same `date_range_expr` on `lower(<col>)` / `upper(<col>)`
/// via `range_endpoint_checks`.
#[model(table = "phase8_5_g0_temporal_inf_date_range", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct TemporalInfDateRangeRow {
    pub validity: Range<::time::Date>,
    pub label: String,
}

/// `Range<time::OffsetDateTime>` column — exercises the TSTZRANGE
/// endpoint CHECKs.
#[model(table = "phase8_5_g0_temporal_inf_tstz_range", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct TemporalInfTstzRangeRow {
    pub booking: Range<::time::OffsetDateTime>,
    pub label: String,
}

// ── (1) — Scalar Date special-value rejection ────────────────────────────────

#[djogi::djogi_test(sync_models = [TemporalInfDateScalarRow])]
async fn scalar_date_check_rejects_positive_infinity(mut ctx: djogi::DjogiContext) {
    // PostgreSQL accepts `DATE 'infinity'` as a valid DATE literal
    // (it's the conventional sentinel for "no upper bound"). The
    // finite-value guard rejects it at the DB layer so subsequent
    // typed reads through `time::Date::from_sql` cannot trip on a
    // value the Rust type has no constructor for.
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_temporal_inf_date_scalar (event_on, label) \
             VALUES (DATE 'infinity', 'scalar-pos-inf')",
            &[],
        )
        .await
        .expect_err("scalar Date CHECK must reject DATE 'infinity'");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_temporal_inf_date_scalar_event_on_check"),
        "+infinity Date INSERT error must reference the structural CHECK constraint name: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [TemporalInfDateScalarRow])]
async fn scalar_date_check_rejects_negative_infinity(mut ctx: djogi::DjogiContext) {
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_temporal_inf_date_scalar (event_on, label) \
             VALUES (DATE '-infinity', 'scalar-neg-inf')",
            &[],
        )
        .await
        .expect_err("scalar Date CHECK must reject DATE '-infinity'");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_temporal_inf_date_scalar_event_on_check"),
        "-infinity Date INSERT error must reference the structural CHECK constraint name: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [TemporalInfDateScalarRow])]
async fn scalar_date_check_accepts_finite_value_round_trip(mut ctx: djogi::DjogiContext) {
    // Accept-path regression guard: an ordinary finite `time::Date`
    // value still round-trips end-to-end through `Model::create` +
    // `Model::get` after the finite guard lands.
    let target = ::time::Date::from_calendar_date(2026, ::time::Month::May, 18).unwrap();
    let row = TemporalInfDateScalarRow::create(
        &mut ctx,
        TemporalInfDateScalarRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            event_on: target,
            label: "scalar-finite".into(),
        },
    )
    .await
    .expect("finite Date value must round-trip after the finite guard lands");

    assert_eq!(row.event_on, target);

    let fetched = TemporalInfDateScalarRow::get(&mut ctx, row.id)
        .await
        .expect("get round-trip");
    assert_eq!(fetched.event_on, target);
}

// ── (2) — Scalar Timestamptz special-value rejection ─────────────────────────

#[djogi::djogi_test(sync_models = [TemporalInfTstzScalarRow])]
async fn scalar_tstz_check_rejects_positive_infinity(mut ctx: djogi::DjogiContext) {
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_temporal_inf_tstz_scalar (recorded_at, label) \
             VALUES (TIMESTAMPTZ 'infinity', 'scalar-pos-inf')",
            &[],
        )
        .await
        .expect_err("scalar Timestamptz CHECK must reject TIMESTAMPTZ 'infinity'");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_temporal_inf_tstz_scalar_recorded_at_check"),
        "+infinity Timestamptz INSERT error must reference the structural CHECK constraint \
         name: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [TemporalInfTstzScalarRow])]
async fn scalar_tstz_check_rejects_negative_infinity(mut ctx: djogi::DjogiContext) {
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_temporal_inf_tstz_scalar (recorded_at, label) \
             VALUES (TIMESTAMPTZ '-infinity', 'scalar-neg-inf')",
            &[],
        )
        .await
        .expect_err("scalar Timestamptz CHECK must reject TIMESTAMPTZ '-infinity'");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_temporal_inf_tstz_scalar_recorded_at_check"),
        "-infinity Timestamptz INSERT error must reference the structural CHECK constraint \
         name: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [TemporalInfTstzScalarRow])]
async fn scalar_tstz_check_accepts_finite_value_round_trip(mut ctx: djogi::DjogiContext) {
    let target = ::time::OffsetDateTime::from_unix_timestamp(1_747_400_000).unwrap();
    let row = TemporalInfTstzScalarRow::create(
        &mut ctx,
        TemporalInfTstzScalarRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            recorded_at: target,
            label: "scalar-finite".into(),
        },
    )
    .await
    .expect("finite Timestamptz value must round-trip after the finite guard lands");

    assert_eq!(row.recorded_at, target);

    let fetched = TemporalInfTstzScalarRow::get(&mut ctx, row.id)
        .await
        .expect("get round-trip");
    assert_eq!(fetched.recorded_at, target);
}

// ── (3) — DATERANGE endpoint special-value rejection ─────────────────────────

#[djogi::djogi_test(sync_models = [TemporalInfDateRangeRow])]
async fn daterange_check_rejects_negative_infinity_in_lower_endpoint(
    mut ctx: djogi::DjogiContext,
) {
    // The lower endpoint check
    // (`lower(validity) IS NULL OR (pg_catalog.isfinite(lower(validity))
    //   AND lower(validity) <= DATE '9999-12-31')`)
    // must reject a DATERANGE whose lower endpoint is `'-infinity'`.
    // Constructed via raw SQL because the typed `Range<time::Date>`
    // surface cannot hold a non-finite value.
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_temporal_inf_date_range (validity, label) \
             VALUES (daterange('-infinity'::date, DATE '2026-12-31', '[)'), \
                     'range-lower-neg-inf')",
            &[],
        )
        .await
        .expect_err("DATERANGE CHECK must reject '-infinity' on the lower endpoint");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_temporal_inf_date_range_validity_check"),
        "-infinity lower-endpoint error must reference the structural CHECK constraint name: \
         {msg}"
    );
}

#[djogi::djogi_test(sync_models = [TemporalInfDateRangeRow])]
async fn daterange_check_rejects_positive_infinity_in_upper_endpoint(
    mut ctx: djogi::DjogiContext,
) {
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_temporal_inf_date_range (validity, label) \
             VALUES (daterange(DATE '2026-01-01', 'infinity'::date, '[]'), \
                     'range-upper-pos-inf')",
            &[],
        )
        .await
        .expect_err("DATERANGE CHECK must reject 'infinity' on the upper endpoint");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_temporal_inf_date_range_validity_check"),
        "+infinity upper-endpoint error must reference the structural CHECK constraint name: \
         {msg}"
    );
}

#[djogi::djogi_test(sync_models = [TemporalInfDateRangeRow])]
async fn daterange_check_accepts_finite_range_round_trip(mut ctx: djogi::DjogiContext) {
    // Accept-path regression guard: a finite DATERANGE still
    // round-trips through the typed surface after the finite-value
    // guard lands on both endpoint checks.
    let lo = ::time::Date::from_calendar_date(2026, ::time::Month::January, 1).unwrap();
    let hi = ::time::Date::from_calendar_date(2026, ::time::Month::December, 31).unwrap();
    let row = TemporalInfDateRangeRow::create(
        &mut ctx,
        TemporalInfDateRangeRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            validity: Range::inclusive_exclusive(lo, hi),
            label: "range-finite".into(),
        },
    )
    .await
    .expect("finite DATERANGE value must round-trip after the finite guard lands");

    assert_eq!(row.label, "range-finite");

    let fetched = TemporalInfDateRangeRow::get(&mut ctx, row.id)
        .await
        .expect("get round-trip on Range<Date>");
    assert_eq!(fetched.label, "range-finite");
}

#[djogi::djogi_test(sync_models = [TemporalInfDateRangeRow])]
async fn daterange_check_accepts_unbounded_lower_range(mut ctx: djogi::DjogiContext) {
    // `range_endpoint_checks` wraps each finite endpoint with `IS NULL
    // OR (...)`; an unbounded-lower range carries NULL `lower(...)`
    // so the finite guard never fires on that side. Pin that path
    // here so the guard doesn't regress unbounded-range support.
    let hi = ::time::Date::from_calendar_date(2026, ::time::Month::December, 31).unwrap();
    let row = TemporalInfDateRangeRow::create(
        &mut ctx,
        TemporalInfDateRangeRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            validity: Range::new(RangeBound::Unbounded, RangeBound::Inclusive(hi)),
            label: "range-lower-unbounded".into(),
        },
    )
    .await
    .expect("unbounded-lower DATERANGE must round-trip");

    assert_eq!(row.label, "range-lower-unbounded");
}

// ── (4) — TSTZRANGE endpoint special-value rejection ─────────────────────────

#[djogi::djogi_test(sync_models = [TemporalInfTstzRangeRow])]
async fn tstzrange_check_rejects_negative_infinity_in_lower_endpoint(
    mut ctx: djogi::DjogiContext,
) {
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_temporal_inf_tstz_range (booking, label) \
             VALUES (tstzrange('-infinity'::timestamptz, \
                               TIMESTAMPTZ '2026-12-31 23:59:59+00', '[)'), \
                     'range-lower-neg-inf')",
            &[],
        )
        .await
        .expect_err("TSTZRANGE CHECK must reject '-infinity' on the lower endpoint");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_temporal_inf_tstz_range_booking_check"),
        "-infinity lower-endpoint error must reference the structural CHECK constraint name: \
         {msg}"
    );
}

#[djogi::djogi_test(sync_models = [TemporalInfTstzRangeRow])]
async fn tstzrange_check_rejects_positive_infinity_in_upper_endpoint(
    mut ctx: djogi::DjogiContext,
) {
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_temporal_inf_tstz_range (booking, label) \
             VALUES (tstzrange(TIMESTAMPTZ '2026-01-01 00:00:00+00', \
                               'infinity'::timestamptz, '[]'), \
                     'range-upper-pos-inf')",
            &[],
        )
        .await
        .expect_err("TSTZRANGE CHECK must reject 'infinity' on the upper endpoint");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_temporal_inf_tstz_range_booking_check"),
        "+infinity upper-endpoint error must reference the structural CHECK constraint name: \
         {msg}"
    );
}

#[djogi::djogi_test(sync_models = [TemporalInfTstzRangeRow])]
async fn tstzrange_check_accepts_finite_range_round_trip(mut ctx: djogi::DjogiContext) {
    let lo = ::time::OffsetDateTime::from_unix_timestamp(1_704_067_200).unwrap();
    let hi = ::time::OffsetDateTime::from_unix_timestamp(1_735_603_200).unwrap();
    let row = TemporalInfTstzRangeRow::create(
        &mut ctx,
        TemporalInfTstzRangeRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            booking: Range::inclusive_exclusive(lo, hi),
            label: "range-finite".into(),
        },
    )
    .await
    .expect("finite TSTZRANGE value must round-trip after the finite guard lands");

    assert_eq!(row.label, "range-finite");

    let fetched = TemporalInfTstzRangeRow::get(&mut ctx, row.id)
        .await
        .expect("get round-trip on Range<OffsetDateTime>");
    assert_eq!(fetched.label, "range-finite");
}

#[djogi::djogi_test(sync_models = [TemporalInfTstzRangeRow])]
async fn tstzrange_check_accepts_unbounded_upper_range(mut ctx: djogi::DjogiContext) {
    let lo = ::time::OffsetDateTime::from_unix_timestamp(1_704_067_200).unwrap();
    let row = TemporalInfTstzRangeRow::create(
        &mut ctx,
        TemporalInfTstzRangeRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            booking: Range::new(RangeBound::Inclusive(lo), RangeBound::Unbounded),
            label: "range-upper-unbounded".into(),
        },
    )
    .await
    .expect("unbounded-upper TSTZRANGE must round-trip");

    assert_eq!(row.label, "range-upper-unbounded");
}

// ── (5) — DATE[] element special-value rejection ─────────────────────────────
//
// The `djogi.__djogi_date_array_is_finite_v1` helper applies
// `pg_catalog.isfinite` per element, rejecting both `-infinity` and
// `+infinity`. The previous `upper_bound >= ALL(col)` check admitted
// `-infinity` because Postgres ordering makes
// `'9999-12-31'::date >= '-infinity'::date` TRUE.

/// `Vec<Date>` column — exercises `date_array_is_finite_check` which
/// projects to `<col> IS NULL OR djogi.__djogi_date_array_is_finite_v1(<col>)`.
/// Uses `Date` (from `djogi::prelude::*`) to match the macro's recognized path forms.
#[model(table = "phase8_5_g0_temporal_inf_date_array", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct TemporalInfDateArrayRow {
    pub dates: Vec<Date>,
    pub label: String,
}

#[djogi::djogi_test(sync_models = [TemporalInfDateArrayRow])]
async fn date_array_check_rejects_negative_infinity_element(mut ctx: djogi::DjogiContext) {
    // `-infinity` is the key gap in the old upper-bound-only strategy:
    // `'9999-12-31'::date >= '-infinity'::date` is TRUE in Postgres, so the
    // old `ALL(col)` check admitted it. The `isfinite`-backed helper rejects it.
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_temporal_inf_date_array (dates, label) \
             VALUES (ARRAY['-infinity'::date], 'date-arr-neg-inf')",
            &[],
        )
        .await
        .expect_err("DATE[] CHECK must reject -infinity element");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_temporal_inf_date_array_dates_check"),
        "-infinity DATE element error must reference the structural CHECK: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [TemporalInfDateArrayRow])]
async fn date_array_check_rejects_positive_infinity_element(mut ctx: djogi::DjogiContext) {
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_temporal_inf_date_array (dates, label) \
             VALUES (ARRAY['infinity'::date], 'date-arr-pos-inf')",
            &[],
        )
        .await
        .expect_err("DATE[] CHECK must reject +infinity element");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_temporal_inf_date_array_dates_check"),
        "+infinity DATE element error must reference the structural CHECK: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [TemporalInfDateArrayRow])]
async fn date_array_check_accepts_finite_elements_round_trip(mut ctx: djogi::DjogiContext) {
    // Accept-path regression guard: a finite `Vec<Date>` still round-trips
    // end-to-end through `Model::create` + `Model::get` after the helper lands.
    let d1 = Date::from_calendar_date(2026, ::time::Month::January, 1).unwrap();
    let d2 = Date::from_calendar_date(2026, ::time::Month::December, 31).unwrap();
    let row = TemporalInfDateArrayRow::create(
        &mut ctx,
        TemporalInfDateArrayRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            dates: vec![d1, d2],
            label: "date-arr-finite".into(),
        },
    )
    .await
    .expect("finite DATE[] must round-trip after the finite-element guard lands");

    assert_eq!(row.dates, vec![d1, d2]);

    let fetched = TemporalInfDateArrayRow::get(&mut ctx, row.id)
        .await
        .expect("get round-trip on Vec<Date>");
    assert_eq!(fetched.dates, vec![d1, d2]);
}

#[djogi::djogi_test(sync_models = [TemporalInfDateArrayRow])]
async fn date_array_check_accepts_empty_array(mut ctx: djogi::DjogiContext) {
    // An empty `Vec<Date>` is a valid value. The helper uses
    // `COALESCE(bool_and(...), true)` so an empty unnest yields true.
    let row = TemporalInfDateArrayRow::create(
        &mut ctx,
        TemporalInfDateArrayRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            dates: vec![],
            label: "date-arr-empty".into(),
        },
    )
    .await
    .expect("empty DATE[] must pass the finite-element check");

    assert_eq!(row.dates, Vec::<Date>::new());
}

// ── (6) — TIMESTAMPTZ[] element special-value rejection ──────────────────────
//
// Same representability class as DATE[]; the underlying failure was identical:
// `'9999-12-31 23:59:59.999999+00'::timestamptz >= '-infinity'::timestamptz`
// is TRUE in Postgres ordering.

/// `Vec<DateTime>` column — exercises `tstz_array_is_finite_check`.
/// Uses `DateTime` (from `djogi::prelude::*`) to match the macro's recognized path forms.
#[model(table = "phase8_5_g0_temporal_inf_tstz_array", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct TemporalInfTstzArrayRow {
    pub timestamps: Vec<DateTime>,
    pub label: String,
}

#[djogi::djogi_test(sync_models = [TemporalInfTstzArrayRow])]
async fn tstz_array_check_rejects_negative_infinity_element(mut ctx: djogi::DjogiContext) {
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_temporal_inf_tstz_array (timestamps, label) \
             VALUES (ARRAY['-infinity'::timestamptz], 'tstz-arr-neg-inf')",
            &[],
        )
        .await
        .expect_err("TIMESTAMPTZ[] CHECK must reject -infinity element");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_temporal_inf_tstz_array_timestamps_check"),
        "-infinity TIMESTAMPTZ element error must reference the structural CHECK: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [TemporalInfTstzArrayRow])]
async fn tstz_array_check_rejects_positive_infinity_element(mut ctx: djogi::DjogiContext) {
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_temporal_inf_tstz_array (timestamps, label) \
             VALUES (ARRAY['infinity'::timestamptz], 'tstz-arr-pos-inf')",
            &[],
        )
        .await
        .expect_err("TIMESTAMPTZ[] CHECK must reject +infinity element");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_temporal_inf_tstz_array_timestamps_check"),
        "+infinity TIMESTAMPTZ element error must reference the structural CHECK: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [TemporalInfTstzArrayRow])]
async fn tstz_array_check_accepts_finite_elements_round_trip(mut ctx: djogi::DjogiContext) {
    let t1 = DateTime::from_unix_timestamp(1_704_067_200).unwrap();
    let t2 = DateTime::from_unix_timestamp(1_747_400_000).unwrap();
    let row = TemporalInfTstzArrayRow::create(
        &mut ctx,
        TemporalInfTstzArrayRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            timestamps: vec![t1, t2],
            label: "tstz-arr-finite".into(),
        },
    )
    .await
    .expect("finite TIMESTAMPTZ[] must round-trip after the finite-element guard lands");

    assert_eq!(row.timestamps, vec![t1, t2]);

    let fetched = TemporalInfTstzArrayRow::get(&mut ctx, row.id)
        .await
        .expect("get round-trip on Vec<DateTime>");
    assert_eq!(fetched.timestamps, vec![t1, t2]);
}

#[djogi::djogi_test(sync_models = [TemporalInfTstzArrayRow])]
async fn tstz_array_check_accepts_empty_array(mut ctx: djogi::DjogiContext) {
    let row = TemporalInfTstzArrayRow::create(
        &mut ctx,
        TemporalInfTstzArrayRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            timestamps: vec![],
            label: "tstz-arr-empty".into(),
        },
    )
    .await
    .expect("empty TIMESTAMPTZ[] must pass the finite-element check");

    assert_eq!(row.timestamps, Vec::<DateTime>::new());
}
