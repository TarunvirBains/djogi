// Phase 8.5 G0 — Decimal representability CHECK rejects PostgreSQL
// NUMERIC special values across every projected surface.
//
// # What this file pins
//
// 1. **Scalar Decimal CHECK rejects `NaN` / `Infinity` / `-Infinity`.**
//    A `rust_decimal::Decimal` column projects
//    `(<col>) IS NULL OR (scale(<col>) IS NOT NULL AND
//     scale(<col>) <= 28 AND
//     abs(<col>) * power(10::numeric, scale(<col>)) <= 2^96 - 1)`. The
//    leading `scale(<col>) IS NOT NULL` clause is the fix for
//    PostgreSQL's three non-finite NUMERIC special values (`NaN`,
//    `+Infinity`, `-Infinity`); `pg_catalog.scale()` is defined to
//    return NULL on each, the `IS NOT NULL` collapses that to FALSE,
//    and the CHECK rejects the write. Without the guard the later
//    `scale(<col>) <= 28` clause NULL-propagates (`NULL <= 28` is
//    NULL) and CHECK's UNKNOWN-treated-as-satisfied rule silently
//    admits the special value — which would later poison a typed
//    `Decimal::from_sql` decode with `DjogiError::Decode`.
// 2. **NUMRANGE endpoint CHECK rejects special-value endpoints.** The
//    `Range<Decimal>` column projects the same `decimal_repr_expr` on
//    both `lower(col)` and `upper(col)`, wrapped by
//    `range_endpoint_checks` with each endpoint's own `IS NULL OR
//    (...)` short-circuit so empty / unbounded / `NULL` ranges remain
//    satisfied. Hand-crafted ranges with `NaN` / `Infinity` /
//    `-Infinity` endpoints are rejected at the DB layer.
// 3. **`NUMERIC[]` helper CHECK rejects special-value elements.** The
//    `djogi.__djogi_numeric_array_is_rust_decimal_v1(values)` helper
//    runs the same `pg_catalog.scale(value) IS NOT NULL` guard inside
//    its per-element `bool_and` predicate, so an array containing any
//    of the three special values fails the CHECK.
// 4. **Finite values still round-trip.** Regular `rust_decimal::Decimal`
//    values continue to pass the CHECK on all three surfaces — the
//    special-value guard does not regress the existing accept path.
//
// # Why a tests/internal target
//
// `rust_decimal::Decimal` cannot construct `NaN`, `Infinity`, or
// `-Infinity` (the type carries no non-finite states), so the only way
// to land a Postgres NUMERIC special value in any of the three columns
// is to hand-craft `INSERT … NUMERIC 'NaN'` / `numrange ('NaN', X)` /
// `ARRAY['NaN'::numeric, ...]` via `raw_execute`. Hand-crafted SQL
// belongs under `tests/internal/`; integration tests stay raw-free
// per CLAUDE.md.
//
// # Spec anchors
//
// - `docs/spec/decisions.md` — "Decimal precision and scale projection
//   (djogi#188)" + "`Range<T>` typed substrate (djogi#148 + djogi#150,
//   Phase 8.5 G0)".
// - `djogi/src/migrate/projection.rs::decimal_repr_expr` — central
//   representability predicate with the `scale IS NOT NULL` guard.
// - `djogi/src/migrate/compose.rs::NUMERIC_ARRAY_HELPER_PRELUDE` —
//   helper SQL body, mirrors the scalar guard inside `bool_and`.

use djogi::prelude::*;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

// ── Test models ──────────────────────────────────────────────────────────────

/// Scalar `Decimal` column — exercises the inline `decimal_repr_expr`
/// CHECK with the `({qcol}) IS NULL OR (...)` outer pass-through wrap.
#[model(table = "phase8_5_g0_decimal_special_scalar", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct DecimalSpecialScalarRow {
    pub amount: Decimal,
    pub label: String,
}

/// `Range<Decimal>` column — exercises the NUMRANGE endpoint CHECKs.
/// The descriptor lowers to `numrange`, and the projection emits the
/// same `decimal_repr_expr` on `lower(<col>)` / `upper(<col>)` via
/// `range_endpoint_checks`.
#[model(table = "phase8_5_g0_decimal_special_range", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct DecimalSpecialRangeRow {
    pub bounds: Range<Decimal>,
    pub label: String,
}

/// `Vec<Decimal>` column — exercises the
/// `djogi.__djogi_numeric_array_is_rust_decimal_v1` helper CHECK with
/// the per-element `pg_catalog.scale(value) IS NOT NULL` guard.
#[model(table = "phase8_5_g0_decimal_special_array", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct DecimalSpecialArrayRow {
    pub amounts: Vec<Decimal>,
    pub label: String,
}

// ── (1) — Scalar Decimal special-value rejection ─────────────────────────────

#[djogi::djogi_test(sync_models = [DecimalSpecialScalarRow])]
async fn scalar_decimal_check_rejects_nan(mut ctx: djogi::DjogiContext) {
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_decimal_special_scalar (amount, label) \
             VALUES (NUMERIC 'NaN', 'scalar-nan')",
            &[],
        )
        .await
        .expect_err("scalar Decimal CHECK must reject NUMERIC 'NaN'");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_decimal_special_scalar_amount_check"),
        "NaN INSERT error must reference the structural CHECK constraint name: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [DecimalSpecialScalarRow])]
async fn scalar_decimal_check_rejects_positive_infinity(mut ctx: djogi::DjogiContext) {
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_decimal_special_scalar (amount, label) \
             VALUES (NUMERIC 'Infinity', 'scalar-pos-inf')",
            &[],
        )
        .await
        .expect_err("scalar Decimal CHECK must reject NUMERIC 'Infinity'");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_decimal_special_scalar_amount_check"),
        "+Infinity INSERT error must reference the structural CHECK constraint name: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [DecimalSpecialScalarRow])]
async fn scalar_decimal_check_rejects_negative_infinity(mut ctx: djogi::DjogiContext) {
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_decimal_special_scalar (amount, label) \
             VALUES (NUMERIC '-Infinity', 'scalar-neg-inf')",
            &[],
        )
        .await
        .expect_err("scalar Decimal CHECK must reject NUMERIC '-Infinity'");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_decimal_special_scalar_amount_check"),
        "-Infinity INSERT error must reference the structural CHECK constraint name: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [DecimalSpecialScalarRow])]
async fn scalar_decimal_check_accepts_finite_value_round_trip(mut ctx: djogi::DjogiContext) {
    // The special-value guard must not regress the accept path: an
    // ordinary `rust_decimal::Decimal` value still round-trips
    // end-to-end through `Model::create` + `Model::get`.
    let row = DecimalSpecialScalarRow::create(
        &mut ctx,
        DecimalSpecialScalarRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            amount: dec!(123.456),
            label: "scalar-finite".into(),
        },
    )
    .await
    .expect("finite Decimal value must round-trip after the special-value guard lands");

    assert_eq!(row.amount, dec!(123.456));

    let fetched = DecimalSpecialScalarRow::get(&mut ctx, row.id)
        .await
        .expect("get round-trip");
    assert_eq!(fetched.amount, dec!(123.456));
}

// ── (2) — NUMRANGE endpoint special-value rejection ──────────────────────────

#[djogi::djogi_test(sync_models = [DecimalSpecialRangeRow])]
async fn numrange_check_rejects_nan_in_lower_endpoint(mut ctx: djogi::DjogiContext) {
    // The lower endpoint check (`lower(bounds) IS NULL OR (scale(lower(
    // bounds)) IS NOT NULL AND ...)`) must reject a NUMRANGE whose
    // lower endpoint is `NaN`. Constructed via raw SQL because the
    // typed `Range<Decimal>` surface cannot hold a non-finite value.
    //
    // Upper bound is NULL (unbounded) rather than a finite value so that
    // PostgreSQL's range constructor does not validate lower <= upper —
    // NaN is ordered GREATER THAN all finite numerics in PostgreSQL, so
    // `numrange('NaN', <finite>, '[)')` fails at construction time with
    // SQLSTATE 22000 before reaching the table CHECK constraint. With an
    // unbounded upper bound the constructor succeeds, `lower(bounds)`
    // returns NaN, `scale(NaN)` returns NULL, and the Djogi structural
    // CHECK fires as intended.
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_decimal_special_range (bounds, label) \
             VALUES (numrange('NaN'::numeric, NULL, '[)'), 'range-lower-nan')",
            &[],
        )
        .await
        .expect_err("NUMRANGE CHECK must reject 'NaN' on the lower endpoint");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_decimal_special_range_bounds_check"),
        "NaN lower-endpoint error must reference the structural CHECK constraint name: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [DecimalSpecialRangeRow])]
async fn numrange_check_rejects_infinity_in_upper_endpoint(mut ctx: djogi::DjogiContext) {
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_decimal_special_range (bounds, label) \
             VALUES (numrange(0::numeric, 'Infinity'::numeric, '[]'), 'range-upper-inf')",
            &[],
        )
        .await
        .expect_err("NUMRANGE CHECK must reject 'Infinity' on the upper endpoint");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_decimal_special_range_bounds_check"),
        "+Infinity upper-endpoint error must reference the structural CHECK constraint name: \
         {msg}"
    );
}

#[djogi::djogi_test(sync_models = [DecimalSpecialRangeRow])]
async fn numrange_check_rejects_negative_infinity_in_lower_endpoint(mut ctx: djogi::DjogiContext) {
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_decimal_special_range (bounds, label) \
             VALUES (numrange('-Infinity'::numeric, 1000::numeric, '[)'), 'range-lower-neg-inf')",
            &[],
        )
        .await
        .expect_err("NUMRANGE CHECK must reject '-Infinity' on the lower endpoint");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_decimal_special_range_bounds_check"),
        "-Infinity lower-endpoint error must reference the structural CHECK constraint name: \
         {msg}"
    );
}

#[djogi::djogi_test(sync_models = [DecimalSpecialRangeRow])]
async fn numrange_check_accepts_finite_range_round_trip(mut ctx: djogi::DjogiContext) {
    // Accept-path regression guard: a finite NUMRANGE still round-trips
    // through the typed surface after the special-value guard lands on
    // both endpoint checks.
    let row = DecimalSpecialRangeRow::create(
        &mut ctx,
        DecimalSpecialRangeRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            bounds: Range::inclusive_exclusive(dec!(1.5), dec!(99.99)),
            label: "range-finite".into(),
        },
    )
    .await
    .expect("finite NUMRANGE value must round-trip after the special-value guard lands");

    assert_eq!(row.label, "range-finite");

    let fetched = DecimalSpecialRangeRow::get(&mut ctx, row.id)
        .await
        .expect("get round-trip on Range<Decimal>");
    assert_eq!(fetched.label, "range-finite");
}

#[djogi::djogi_test(sync_models = [DecimalSpecialRangeRow])]
async fn numrange_check_accepts_unbounded_range(mut ctx: djogi::DjogiContext) {
    // `range_endpoint_checks` wraps each finite endpoint with `IS NULL
    // OR (...)`; an unbounded range carries NULL endpoints so the
    // special-value guard never fires. Pin that path here so the guard
    // doesn't regress unbounded-range support.
    let row = DecimalSpecialRangeRow::create(
        &mut ctx,
        DecimalSpecialRangeRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            bounds: Range::new(RangeBound::Inclusive(dec!(0)), RangeBound::Unbounded),
            label: "range-upper-unbounded".into(),
        },
    )
    .await
    .expect("unbounded-upper NUMRANGE must round-trip");

    assert_eq!(row.label, "range-upper-unbounded");
}

// ── (3) — NUMERIC[] helper special-value rejection ───────────────────────────

#[djogi::djogi_test(sync_models = [DecimalSpecialArrayRow])]
async fn numeric_array_helper_rejects_nan_element(mut ctx: djogi::DjogiContext) {
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_decimal_special_array (amounts, label) \
             VALUES (ARRAY[1::numeric, 'NaN'::numeric, 3::numeric]::numeric[], 'arr-nan')",
            &[],
        )
        .await
        .expect_err(
            "NUMERIC[] helper CHECK must reject an array containing 'NaN' via the \
             per-element scale IS NOT NULL guard",
        );

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_decimal_special_array_amounts_check"),
        "NaN-element error must reference the array CHECK constraint name: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [DecimalSpecialArrayRow])]
async fn numeric_array_helper_rejects_infinity_element(mut ctx: djogi::DjogiContext) {
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_decimal_special_array (amounts, label) \
             VALUES (ARRAY[42::numeric, 'Infinity'::numeric]::numeric[], 'arr-inf')",
            &[],
        )
        .await
        .expect_err(
            "NUMERIC[] helper CHECK must reject an array containing 'Infinity' via the \
             per-element scale IS NOT NULL guard",
        );

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_decimal_special_array_amounts_check"),
        "+Infinity-element error must reference the array CHECK constraint name: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [DecimalSpecialArrayRow])]
async fn numeric_array_helper_rejects_negative_infinity_element(mut ctx: djogi::DjogiContext) {
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_g0_decimal_special_array (amounts, label) \
             VALUES (ARRAY[0::numeric, '-Infinity'::numeric, 7::numeric]::numeric[], 'arr-neg-inf')",
            &[],
        )
        .await
        .expect_err(
            "NUMERIC[] helper CHECK must reject an array containing '-Infinity' via the \
             per-element scale IS NOT NULL guard",
        );

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_g0_decimal_special_array_amounts_check"),
        "-Infinity-element error must reference the array CHECK constraint name: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [DecimalSpecialArrayRow])]
async fn numeric_array_helper_accepts_finite_elements_round_trip(mut ctx: djogi::DjogiContext) {
    // Accept-path regression guard: a Vec<Decimal> with only finite
    // values must continue to round-trip through the helper-backed
    // CHECK after the per-element special-value guard lands.
    let row = DecimalSpecialArrayRow::create(
        &mut ctx,
        DecimalSpecialArrayRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            amounts: vec![dec!(1.5), dec!(99.99), dec!(0)],
            label: "arr-finite".into(),
        },
    )
    .await
    .expect("finite NUMERIC[] values must round-trip after the special-value guard lands");

    assert_eq!(row.amounts, vec![dec!(1.5), dec!(99.99), dec!(0)]);

    let fetched = DecimalSpecialArrayRow::get(&mut ctx, row.id)
        .await
        .expect("get round-trip on Vec<Decimal>");
    assert_eq!(fetched.amounts, row.amounts);
}

#[djogi::djogi_test(sync_models = [DecimalSpecialArrayRow])]
async fn numeric_array_helper_accepts_empty_array(mut ctx: djogi::DjogiContext) {
    // `COALESCE(bool_and(...), true)` accepts an empty array — the
    // `bool_and` aggregate returns NULL over zero rows, and the
    // `COALESCE` short-circuits to TRUE. The special-value guard does
    // not regress that path.
    let row = DecimalSpecialArrayRow::create(
        &mut ctx,
        DecimalSpecialArrayRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            amounts: Vec::new(),
            label: "arr-empty".into(),
        },
    )
    .await
    .expect("empty NUMERIC[] must round-trip after the special-value guard lands");

    assert_eq!(row.amounts, Vec::<Decimal>::new());
}
