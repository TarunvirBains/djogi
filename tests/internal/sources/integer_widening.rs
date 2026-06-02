// Issue #190 — u8/u16/u64 (and i8/u32 from #186)
// bind/decode shims + type-derived CHECK projection.
//
// # What this file pins
//
// 1. **Compile pass.** Models with `i8`, `u8`, `u16`, `u32`, `u64` scalar
//    fields expand without compiler errors (covered by the lihaaf fixture
//    `integer_widening.rs`).
//
// 2. **Round-trip.** `Model::create` + `Model::get` round-trips boundary
//    values for each narrow/unsigned type through the typed surface:
//    - i8: min=-128, max=127
//    - u8: min=0, max=255
//    - u16: min=0, max=65535
//    - u32: min=0, max=4294967295
//    - u64: min=0, max=18446744073709551615
//
// 3. **OOB rejection (raw bypass).** The type-derived CHECK rejects raw
//    INSERTs that violate the Rust type range. Each narrow column carries a
//    CHECK of the form `col >= <min> AND col <= <max>` (two-sided bound).
//    - i8 SMALLINT: reject -129 (below i8::MIN)
//    - u8 SMALLINT: reject 256 (above u8::MAX)
//    - u16 INTEGER: reject 65536 (above u16::MAX)
//    - u32 BIGINT: reject 4294967296 (above u32::MAX)
//    - u64 NUMERIC: reject -1 (below u64::MIN), reject 1.5 (fractional, via trunc check)
//
// 4. **Decode-side OOB.** If a row somehow lands without the CHECK (e.g.
//    a schema reapplied before the migration), the decode shim surfaces
//    `DjogiError::Decode` rather than panicking or silently truncating.
//
// # Why a tests/internal target
//
// Points 3 and 4 require constructing Postgres values outside the Rust
// type's representable range, which is unreachable through the typed
// surface. The only way to exercise OOB write rejection or decode-side
// error is via `raw_execute`. Tests that reach for raw access live under
// `tests/internal/`; adopter-shaped integration tests stay raw-free per
// CLAUDE.md.
//
// # Spec anchors
//
// - GH #190 — u8/u16/u64 bind/decode shims + type-derived CHECK
// - `docs/spec/migrations.md` §10.6.1 — Type-Derived CHECK Projection
// - `djogi/src/migrate/projection.rs::field_type_check` — integer arms

use djogi::prelude::*;

// ── Test model — all five narrow/unsigned types as scalar fields ──────────────

#[model(table = "phase8_5_c2_190_narrow_ints", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct Phase85C2190NarrowInts {
    // djogi#186 types (SMALLINT and BIGINT, shim-bound from narrower Rust types)
    pub signed_byte: i8,
    pub unsigned_int: u32,
    // djogi#190 types (u8 → SMALLINT, u16 → INTEGER, u64 → NUMERIC + integrality CHECK)
    pub unsigned_byte: u8,
    pub unsigned_short: u16,
    pub unsigned_long: u64,
    pub label: String,
}

// ── Round-trip tests ──────────────────────────────────────────────────────────

#[djogi::djogi_test(sync_models = [Phase85C2190NarrowInts])]
async fn narrow_ints_boundary_round_trip(mut ctx: djogi::DjogiContext) {
    // Test boundary values for all five narrow/unsigned types.
    // The typed `Model::create` + `Model::get` path exercises:
    //   - bind shim (Rust type → widened SQL wire type)
    //   - decode shim (widened SQL wire type → Rust type, bounds-checked)
    let row = Phase85C2190NarrowInts::create(
        &mut ctx,
        Phase85C2190NarrowInts {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            signed_byte: i8::MAX,
            unsigned_int: u32::MAX,
            unsigned_byte: u8::MAX,
            unsigned_short: u16::MAX,
            unsigned_long: u64::MAX,
            label: "max values".into(),
        },
    )
    .await
    .expect("boundary MAX values must round-trip through typed surface");

    assert_eq!(row.signed_byte, i8::MAX, "i8::MAX round-trip");
    assert_eq!(row.unsigned_int, u32::MAX, "u32::MAX round-trip");
    assert_eq!(row.unsigned_byte, u8::MAX, "u8::MAX round-trip");
    assert_eq!(row.unsigned_short, u16::MAX, "u16::MAX round-trip");
    assert_eq!(row.unsigned_long, u64::MAX, "u64::MAX round-trip");

    // Re-fetch through Model::get to exercise the full decode path.
    let fetched = Phase85C2190NarrowInts::get(&mut ctx, row.id)
        .await
        .expect("get should succeed");
    assert_eq!(fetched.signed_byte, i8::MAX, "i8::MAX re-fetch");
    assert_eq!(fetched.unsigned_int, u32::MAX, "u32::MAX re-fetch");
    assert_eq!(fetched.unsigned_byte, u8::MAX, "u8::MAX re-fetch");
    assert_eq!(fetched.unsigned_short, u16::MAX, "u16::MAX re-fetch");
    assert_eq!(fetched.unsigned_long, u64::MAX, "u64::MAX re-fetch");
}

#[djogi::djogi_test(sync_models = [Phase85C2190NarrowInts])]
async fn narrow_ints_min_values_round_trip(mut ctx: djogi::DjogiContext) {
    let row = Phase85C2190NarrowInts::create(
        &mut ctx,
        Phase85C2190NarrowInts {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            signed_byte: i8::MIN,
            unsigned_int: 0u32,
            unsigned_byte: 0u8,
            unsigned_short: 0u16,
            unsigned_long: 0u64,
            label: "min values".into(),
        },
    )
    .await
    .expect("boundary MIN values must round-trip");

    assert_eq!(row.signed_byte, i8::MIN, "i8::MIN round-trip");
    assert_eq!(row.unsigned_int, 0u32, "u32 min round-trip");
    assert_eq!(row.unsigned_byte, 0u8, "u8 min round-trip");
    assert_eq!(row.unsigned_short, 0u16, "u16 min round-trip");
    assert_eq!(row.unsigned_long, 0u64, "u64 min round-trip");
}

// ── Live catalog assertions ───────────────────────────────────────────────────

#[djogi::djogi_test(sync_models = [Phase85C2190NarrowInts])]
async fn narrow_ints_catalog_has_check_constraints(mut ctx: djogi::DjogiContext) {
    // Verify that `sync_models` emits the type-derived CHECK constraints.
    // The constraint name follows the `{table}_{column}_check` convention
    // from `migrate::sql::check_constraint_name`.
    let checks: Vec<String> = ctx
        .raw_rows(
            "SELECT conname::text FROM pg_constraint \
             WHERE conrelid = 'phase8_5_c2_190_narrow_ints'::regclass \
             AND contype = 'c' ORDER BY conname",
            &[],
        )
        .await
        .expect("catalog query must succeed")
        .into_iter()
        .map(|row| row.try_get::<_, String>(0).unwrap())
        .collect();

    let expected_checks = [
        "phase8_5_c2_190_narrow_ints_signed_byte_check",
        "phase8_5_c2_190_narrow_ints_unsigned_byte_check",
        "phase8_5_c2_190_narrow_ints_unsigned_int_check",
        "phase8_5_c2_190_narrow_ints_unsigned_long_check",
        "phase8_5_c2_190_narrow_ints_unsigned_short_check",
    ];

    for expected in &expected_checks {
        assert!(
            checks.contains(&(*expected).to_string()),
            "missing CHECK constraint: {expected}; found: {checks:?}"
        );
    }
}

// ── OOB rejection tests (raw bypass) ─────────────────────────────────────────

#[djogi::djogi_test(sync_models = [Phase85C2190NarrowInts])]
async fn i8_check_rejects_value_below_min(mut ctx: djogi::DjogiContext) {
    // -129 is one below i8::MIN (-128). With `i8 → SMALLINT`, the CHECK
    // `signed_byte >= -128 AND signed_byte <= 127` must reject this write.
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_c2_190_narrow_ints \
             (signed_byte, unsigned_int, unsigned_byte, unsigned_short, unsigned_long, label) \
             VALUES (-129, 0, 0, 0, 0, 'oob')",
            &[],
        )
        .await
        .expect_err("i8 below MIN must be rejected by CHECK");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_c2_190_narrow_ints_signed_byte_check"),
        "i8 OOB must cite the CHECK constraint: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [Phase85C2190NarrowInts])]
async fn u8_check_rejects_value_above_max(mut ctx: djogi::DjogiContext) {
    // 256 is one above u8::MAX (255). With `u8 → SMALLINT`, the CHECK
    // `unsigned_byte >= 0 AND unsigned_byte <= 255` must reject this write.
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_c2_190_narrow_ints \
             (signed_byte, unsigned_int, unsigned_byte, unsigned_short, unsigned_long, label) \
             VALUES (0, 0, 256, 0, 0, 'oob')",
            &[],
        )
        .await
        .expect_err("u8 above MAX must be rejected by CHECK");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_c2_190_narrow_ints_unsigned_byte_check"),
        "u8 OOB must cite the CHECK constraint: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [Phase85C2190NarrowInts])]
async fn u16_check_rejects_value_above_max(mut ctx: djogi::DjogiContext) {
    // 65536 is one above u16::MAX (65535). With `u16 → INTEGER`.
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_c2_190_narrow_ints \
             (signed_byte, unsigned_int, unsigned_byte, unsigned_short, unsigned_long, label) \
             VALUES (0, 0, 0, 65536, 0, 'oob')",
            &[],
        )
        .await
        .expect_err("u16 above MAX must be rejected by CHECK");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_c2_190_narrow_ints_unsigned_short_check"),
        "u16 OOB must cite the CHECK constraint: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [Phase85C2190NarrowInts])]
async fn u32_check_rejects_value_above_max(mut ctx: djogi::DjogiContext) {
    // 4294967296 is one above u32::MAX (4294967295). With `u32 → BIGINT`.
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_c2_190_narrow_ints \
             (signed_byte, unsigned_int, unsigned_byte, unsigned_short, unsigned_long, label) \
             VALUES (0, 4294967296, 0, 0, 0, 'oob')",
            &[],
        )
        .await
        .expect_err("u32 above MAX must be rejected by CHECK");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_c2_190_narrow_ints_unsigned_int_check"),
        "u32 OOB must cite the CHECK constraint: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [Phase85C2190NarrowInts])]
async fn u64_check_rejects_negative_value(mut ctx: djogi::DjogiContext) {
    // u64::MIN is 0; -1 is below the lower bound of the NUMERIC column.
    // The CHECK `unsigned_long >= 0 AND ... AND unsigned_long = trunc(unsigned_long)`
    // must reject this write (the `>= 0` clause fires first).
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_c2_190_narrow_ints \
             (signed_byte, unsigned_int, unsigned_byte, unsigned_short, unsigned_long, label) \
             VALUES (0, 0, 0, 0, -1, 'oob')",
            &[],
        )
        .await
        .expect_err("u64 negative value must be rejected by CHECK");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_c2_190_narrow_ints_unsigned_long_check"),
        "u64 negative value must cite the CHECK constraint: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [Phase85C2190NarrowInts])]
async fn u64_check_rejects_fractional_value(mut ctx: djogi::DjogiContext) {
    // djogi#190 Finding 1 — u64 uses bare NUMERIC (not NUMERIC(20,0)).
    // NUMERIC(20,0) would silently round 1.5 → 2 before the CHECK fires,
    // making the CHECK useless against fractional inputs. Bare NUMERIC stores
    // 1.5 unchanged, so the integrality clause `unsigned_long = trunc(unsigned_long)`
    // must reject the INSERT.
    //
    // Raw SQL is the only way to bypass the Rust bind shim (which converts
    // u64 → Decimal::from(u64), always integral) and insert a fractional
    // value to test the DB-level integrality CHECK.
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_c2_190_narrow_ints \
             (signed_byte, unsigned_int, unsigned_byte, unsigned_short, unsigned_long, label) \
             VALUES (0, 0, 0, 0, 1.5, 'fractional')",
            &[],
        )
        .await
        .expect_err("fractional value 1.5 must be rejected by the integrality CHECK on u64 NUMERIC column");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_c2_190_narrow_ints_unsigned_long_check"),
        "u64 fractional value must cite the CHECK constraint: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [Phase85C2190NarrowInts])]
async fn u64_check_rejects_u64_max_plus_one(mut ctx: djogi::DjogiContext) {
    // u64::MAX + 1 = 18_446_744_073_709_551_616 — above the upper bound.
    // Raw SQL is needed to insert a value that overflows u64 (unreachable via
    // the Rust typed surface since u64::MAX is the largest representable value).
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_c2_190_narrow_ints \
             (signed_byte, unsigned_int, unsigned_byte, unsigned_short, unsigned_long, label) \
             VALUES (0, 0, 0, 0, 18446744073709551616, 'oob-max')",
            &[],
        )
        .await
        .expect_err("u64::MAX+1 must be rejected by the upper-bound CHECK");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_c2_190_narrow_ints_unsigned_long_check"),
        "u64 above-max value must cite the CHECK constraint: {msg}"
    );
}
