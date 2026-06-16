// djogi#105 (`#[field(check)]`) and djogi#188
// (Decimal structural CHECK).
//
// # What this file pins
//
// 1. **Round-trip with `#[field(check)]`.** An adopter CHECK that passes
//    on valid data round-trips through `Model::create` + `Model::get`.
// 2. **`#[field(check)]` rejection.** An adopter CHECK that fails on
//    invalid data rejects the typed `Model::create` call at write time
//    via Postgres's CHECK violation surfaced as `DjogiError::Db`.
// 3. **Decimal round-trip.** A `rust_decimal::Decimal` column with the
//    full set of representable values (negative, zero, scale 0..28,
//    integer max, fractional) round-trips through the typed surface
//    — the structural CHECK accepts every rust_decimal value.
// 4. **Decimal OOB rejection (scale).** A raw INSERT with scale > 28
//    is rejected at the DB layer with a CHECK violation.
// 5. **Decimal OOB rejection (magnitude).** A raw INSERT whose
//    coefficient exceeds 2^96 - 1 is rejected.
// 6. **Combined CHECK.** A `u32` column with both the type-derived
//    range CHECK and an adopter `#[field(check = "port > 0")]`: the
//    combined constraint enforces BOTH clauses; violating either
//    rejects the write.
// 7. **Catalog assertions.** `sync_models` emits exactly the expected
//    CHECK constraints (one per column) and the constraint expressions
//    contain both the type-derived clause and the adopter clause for
//    the combined case.
//
// # Why a tests/internal target
//
// rust_decimal's typed surface caps at the representable range, so the
// only way to produce a value with scale 50 or a 30-digit integer is
// to hand-craft the NUMERIC literal via `raw_execute`. The Decimal
// OOB rejection tests are the structural-CHECK equivalent of the
// temporal year >9999 rejections in #187 — both probe the type-derived
// CHECK with values unreachable via the typed Rust path.
//
// # Spec anchors
//
// - GH #105 — `#[field(check = "<sql>")]` adopter CHECK constraint.
// - GH #188 — auto-emit precision/scale CHECK for Decimal columns.
// - `docs/spec/migrations.md` §10.6.1 — Type-Derived CHECK Projection
//   (Decimal arm + adopter combination contract).
// - `djogi/src/migrate/projection.rs::field_type_check` — Numeric arm
//   for `RustSourceType::Decimal`.
// - `djogi/src/migrate/projection.rs::combine_check_expressions` —
//   AND-merge of type-derived and adopter CHECKs.

use djogi::prelude::*;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

// ── Models ────────────────────────────────────────────────────────────────────

/// Decimal column with no adopter CHECK — exercises the type-derived
/// structural bound only (djogi#188).
#[model(table = "decimal_rows", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct DecimalRow {
    pub amount: Decimal,
    pub label: String,
}

/// Adopter CHECK on a String column — exercises the djogi#105 path with
/// no type-derived CHECK to combine.
#[model(table = "check_rows", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct CheckRow {
    pub name: String,
    /// Adopter CHECK: weight must be positive. The Rust source type
    /// `f64` lowers to `DOUBLE PRECISION` with no type-derived CHECK,
    /// so the constraint is precisely the adopter expression.
    #[field(check = "weight_kg > 0")]
    pub weight_kg: f64,
}

/// Combined type-derived (u32 range) + adopter CHECK — exercises the
/// projection layer's AND-merge contract.
#[model(table = "check_combined_rows", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct CombinedRow {
    /// `u32` projects `port >= 0 AND port <= 4294967295` (djogi#190).
    /// Adopter overlay: `port > 0` (no port 0 — zero is the
    /// "no preference" sentinel in some networking APIs, but the
    /// adopter wants every row to carry a real bound).
    #[field(check = "port > 0")]
    pub port: u32,
    pub label: String,
}

// ── (1) — Decimal round-trip across the representable range ──────────────────

#[djogi::djogi_test(sync_models = [DecimalRow])]
async fn decimal_check_accepts_full_representable_range(mut ctx: djogi::DjogiContext) {
    // Sample values across the rust_decimal representable range:
    //   - Zero
    //   - Negative magnitude with scale 0
    //   - Positive magnitude with mid-scale
    //   - Maximum-scale fractional value (28 places)
    //   - Largest integer magnitude (29 sig digits at scale 0)
    let samples = [
        ("zero", dec!(0)),
        ("negative-int", dec!(-1234567890)),
        ("currency", dec!(49.99)),
        ("max-scale", dec!(0.1234567890123456789012345678)),
        // rust_decimal::MAX = 79228162514264337593543950335 (2^96 - 1, scale 0).
        ("max-magnitude", Decimal::MAX),
    ];
    for (label, value) in samples {
        let row = DecimalRow::create(
            &mut ctx,
            DecimalRow {
                id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
                created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                amount: value,
                label: label.into(),
            },
        )
        .await
        .unwrap_or_else(|e| {
            panic!("rust_decimal value {value} ({label}) must round-trip; got {e:?}")
        });
        assert_eq!(row.amount, value, "{label} value round-trip");

        // Re-fetch to exercise FromPgRow decode through the structural CHECK.
        let fetched = DecimalRow::get(&mut ctx, row.id)
            .await
            .expect("get must succeed");
        assert_eq!(fetched.amount, value, "{label} value re-fetch");
    }
}

// ── (2) — Decimal scale OOB rejection ────────────────────────────────────────

#[djogi::djogi_test(sync_models = [DecimalRow])]
async fn decimal_check_rejects_scale_above_28(mut ctx: djogi::DjogiContext) {
    // Raw NUMERIC literal with scale 30 — rust_decimal cannot construct
    // this (Decimal's scale field is capped at 28), so the only way to
    // land a value with scale > 28 is via raw SQL. The structural CHECK
    // `scale(amount) <= 28` rejects this at write time.
    let err = ctx
        .raw_execute(
            "INSERT INTO decimal_rows (amount, label) \
             VALUES (NUMERIC '0.123456789012345678901234567890', 'oob-scale')",
            &[],
        )
        .await
        .expect_err("scale 30 must be rejected by the Decimal structural CHECK");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("decimal_rows_amount_check"),
        "scale-OOB error must reference the structural CHECK constraint name: {msg}"
    );
}

// ── (3) — Decimal magnitude OOB rejection ────────────────────────────────────

#[djogi::djogi_test(sync_models = [DecimalRow])]
async fn decimal_check_rejects_coefficient_above_2pow96(mut ctx: djogi::DjogiContext) {
    // Raw NUMERIC literal with 30 integer digits — coefficient is
    // 100_000_000_000_000_000_000_000_000_000 (10^29), which exceeds
    // 2^96 - 1 = 79_228_162_514_264_337_593_543_950_335 (29 digits).
    // rust_decimal cannot construct this value (mantissa overflow);
    // raw SQL is the only path. The structural CHECK
    // `abs(amount) * power(10::numeric, scale(amount)) <= 2^96 - 1`
    // rejects it.
    let err = ctx
        .raw_execute(
            "INSERT INTO decimal_rows (amount, label) \
             VALUES (NUMERIC '100000000000000000000000000000', 'oob-mag')",
            &[],
        )
        .await
        .expect_err("coefficient > 2^96 - 1 must be rejected by the Decimal structural CHECK");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("decimal_rows_amount_check"),
        "magnitude-OOB error must reference the structural CHECK constraint name: {msg}"
    );
}

// ── (4) — Adopter CHECK pass + reject through the typed surface ──────────────

#[djogi::djogi_test(sync_models = [CheckRow])]
async fn field_check_passes_on_valid_data(mut ctx: djogi::DjogiContext) {
    // `weight_kg > 0` permits any positive value.
    let row = CheckRow::create(
        &mut ctx,
        CheckRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            name: "cat".into(),
            weight_kg: 4.5,
        },
    )
    .await
    .expect("positive weight passes the adopter CHECK");
    assert_eq!(row.weight_kg, 4.5);
}

#[djogi::djogi_test(sync_models = [CheckRow])]
async fn field_check_rejects_invalid_data_through_typed_surface(mut ctx: djogi::DjogiContext) {
    // `weight_kg > 0` rejects zero and negative values. The typed
    // `Model::create` path can produce these values (f64 is the
    // adopter's Rust type, not bounded by the framework), so the
    // adopter CHECK is the only line of defence against invalid
    // input — and it fires at the DB layer rather than burying the
    // problem in application code.
    let err = CheckRow::create(
        &mut ctx,
        CheckRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            name: "ghost".into(),
            weight_kg: 0.0,
        },
    )
    .await
    .expect_err("zero weight must be rejected by the adopter CHECK");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("check_rows_weight_kg_check"),
        "adopter CHECK violation must reference the constraint name: {msg}"
    );
}

// ── (5) — Combined type-derived + adopter CHECK ──────────────────────────────

#[djogi::djogi_test(sync_models = [CombinedRow])]
async fn combined_check_accepts_value_satisfying_both_clauses(mut ctx: djogi::DjogiContext) {
    // 8080 satisfies BOTH the u32 range (0..=4294967295) AND the
    // adopter clause (port > 0).
    let row = CombinedRow::create(
        &mut ctx,
        CombinedRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            port: 8080,
            label: "http-alt".into(),
        },
    )
    .await
    .expect("8080 satisfies both clauses of the combined CHECK");
    assert_eq!(row.port, 8080);
}

#[djogi::djogi_test(sync_models = [CombinedRow])]
async fn combined_check_rejects_violation_of_adopter_clause(mut ctx: djogi::DjogiContext) {
    // 0 satisfies the u32 range (0 is the lower bound) but VIOLATES
    // the adopter clause (port > 0). The combined `(<u32 range>) AND
    // (<adopter>)` constraint rejects the write because the second
    // clause fails.
    let err = CombinedRow::create(
        &mut ctx,
        CombinedRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            port: 0,
            label: "zero".into(),
        },
    )
    .await
    .expect_err("port 0 violates the adopter clause and must be rejected");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("check_combined_rows_port_check"),
        "combined CHECK violation must reference the single constraint name: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [CombinedRow])]
async fn combined_check_rejects_violation_of_type_clause(mut ctx: djogi::DjogiContext) {
    // 4294967296 (u32::MAX + 1) VIOLATES the type-derived range but the
    // typed Rust surface can't reach it (`u32` is bounded). We must use
    // raw_execute to construct the value. The combined `(<u32 range>)
    // AND (<adopter>)` constraint rejects the write because the first
    // clause fails.
    let err = ctx
        .raw_execute(
            "INSERT INTO check_combined_rows (port, label) \
             VALUES (4294967296, 'overflow')",
            &[],
        )
        .await
        .expect_err("port above u32::MAX must be rejected by the type-derived clause");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("check_combined_rows_port_check"),
        "combined CHECK violation (type clause) must reference the single constraint name: {msg}"
    );
}

// ── (6) — Catalog assertions ─────────────────────────────────────────────────

#[djogi::djogi_test(sync_models = [DecimalRow, CheckRow, CombinedRow])]
async fn catalog_has_expected_check_constraints(mut ctx: djogi::DjogiContext) {
    // Decimal row: one CHECK on `amount`.
    let amount_check = ctx
        .raw_rows(
            "SELECT pg_get_constraintdef(c.oid) FROM pg_constraint c \
             WHERE c.conrelid = 'decimal_rows'::regclass \
             AND c.contype = 'c' AND c.conname = 'decimal_rows_amount_check'",
            &[],
        )
        .await
        .expect("catalog query must succeed");
    assert_eq!(amount_check.len(), 1, "exactly one CHECK on amount");
    let amount_def: String = amount_check[0].try_get(0).unwrap();
    assert!(
        amount_def.contains("scale(amount)") || amount_def.contains("scale((amount))"),
        "Decimal CHECK must reference scale(amount): {amount_def}"
    );
    assert!(
        amount_def.contains("79228162514264337593543950335"),
        "Decimal CHECK upper bound must be 2^96 - 1: {amount_def}"
    );

    // Adopter-only CHECK row: one CHECK on `weight_kg`, contains only
    // the adopter expression (no combine wrapping because no type-derived
    // CHECK exists for f64 / DOUBLE PRECISION).
    let weight_check = ctx
        .raw_rows(
            "SELECT pg_get_constraintdef(c.oid) FROM pg_constraint c \
             WHERE c.conrelid = 'check_rows'::regclass \
             AND c.contype = 'c' AND c.conname = 'check_rows_weight_kg_check'",
            &[],
        )
        .await
        .expect("catalog query must succeed");
    assert_eq!(weight_check.len(), 1, "exactly one CHECK on weight_kg");
    let weight_def: String = weight_check[0].try_get(0).unwrap();
    assert!(
        weight_def.contains("weight_kg > (0)::double precision")
            || weight_def.contains("weight_kg > 0"),
        "adopter CHECK must contain the verbatim expression (Postgres may parenthesize): {weight_def}"
    );

    // Combined CHECK row: one CHECK on `port` containing BOTH the
    // type-derived range and the adopter clause.
    let port_check = ctx
        .raw_rows(
            "SELECT pg_get_constraintdef(c.oid) FROM pg_constraint c \
             WHERE c.conrelid = 'check_combined_rows'::regclass \
             AND c.contype = 'c' AND c.conname = 'check_combined_rows_port_check'",
            &[],
        )
        .await
        .expect("catalog query must succeed");
    assert_eq!(port_check.len(), 1, "exactly one CHECK on port");
    let port_def: String = port_check[0].try_get(0).unwrap();
    assert!(
        port_def.contains("4294967295"),
        "combined CHECK must include the u32 upper bound: {port_def}"
    );
    assert!(
        port_def.contains("port > 0"),
        "combined CHECK must include the adopter clause: {port_def}"
    );
    assert!(
        port_def.contains("AND"),
        "combined CHECK must AND the two clauses: {port_def}"
    );
}
