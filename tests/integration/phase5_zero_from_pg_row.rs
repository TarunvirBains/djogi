//! Phase 5-Zero T3 — public `FromPgRow` trait, ordinal decode, and the
//! debug-build column-name guard.
//!
//! These tests pin three contracts the T3 landing introduces:
//!
//! 1. `djogi::FromPgRow` is a public trait (not a crate-private bridge).
//!    Anything that used to bound on the now-retired `FromPgRowBridge`
//!    must continue to compile after renaming the bound.
//! 2. `<T as FromPgRow>::COLUMNS` exposes the canonical select column
//!    order the macro bakes in. The same list, joined with `", "`, is
//!    available as `<T as FromPgRow>::COLUMN_LIST` and is what
//!    `SELECT {COLUMN_LIST}` / `RETURNING {COLUMN_LIST}` now emit.
//! 3. `from_pg_row` decodes positionally — `row.try_get(0)`,
//!    `row.try_get(1)`, etc. — and the emitted `debug_assert_eq!` on
//!    `row.columns()[i].name()` panics under `cargo test` if the row
//!    shape drifts from the expected struct-field order.
//!
//! The ordinal-decode guard test deliberately SELECTs columns in a
//! different order than `COLUMNS` declares and asserts that
//! `from_pg_row` panics (debug builds only). Release builds skip the
//! guard, so the test is gated on `cfg(debug_assertions)`; the crate's
//! `cargo test --workspace` runs in debug mode, matching the plan's
//! exit contract.

#![allow(deprecated)]

use djogi::FromPgRow;
use djogi::prelude::*;
use serde::{Deserialize, Serialize};

// A minimal #[model] struct with three user columns so the COLUMN_LIST
// test has something distinctive to pin (not just the framework fields).
#[model(table = "t3_probes")]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct T3Probe {
    pub label: String,
    pub count: i32,
    pub flag: bool,
}

/// Canonical column list baked in at macro time matches struct field
/// order after framework-field injection: id, created_at, updated_at,
/// then the user fields in declaration order.
#[test]
fn columns_in_struct_field_order() {
    assert_eq!(
        <T3Probe as FromPgRow>::COLUMNS,
        &["id", "created_at", "updated_at", "label", "count", "flag"],
    );
}

/// `COLUMN_LIST` is the same slice joined with `", "`. This is the
/// exact string the macro embeds into `SELECT {COLUMN_LIST} FROM t`
/// and `RETURNING {COLUMN_LIST}` rather than the old `SELECT *` /
/// `RETURNING *`. Pinning the text prevents drift between the const
/// and the SQL it's meant to mirror.
#[test]
fn column_list_is_comma_joined() {
    assert_eq!(
        <T3Probe as FromPgRow>::COLUMN_LIST,
        "id, created_at, updated_at, label, count, flag",
    );
}

/// Round-trip test for the ordinal-decode happy path. `T3Probe::get`
/// (which issues `SELECT {COLUMN_LIST} FROM t3_probes WHERE id = $1`)
/// must decode positionally via `FromPgRow::from_pg_row` without the
/// debug-assert firing.
#[djogi::djogi_test]
async fn from_pg_row_round_trips_ordinal_decode(mut ctx: djogi::DjogiContext) {
    setup_probe_table(&mut ctx).await;

    let created = T3Probe::create(
        &mut ctx,
        T3Probe {
            label: "hello".into(),
            count: 42,
            flag: true,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    assert_eq!(created.label, "hello");
    assert_eq!(created.count, 42);
    assert!(created.flag);
    assert_ne!(
        created.id.as_i64(),
        0,
        "id should be populated via RETURNING"
    );

    let fetched = T3Probe::get(&mut ctx, created.id)
        .await
        .expect("get should succeed");
    assert_eq!(fetched.label, "hello");
    assert_eq!(fetched.count, 42);
    assert!(fetched.flag);
}

/// In debug builds the macro emits `debug_assert_eq!(row.columns()[i].name(), ...)`
/// per column so any drift between the SELECT shape and the struct
/// field order panics loudly rather than silently mis-decoding. This
/// test sends a hand-rolled SELECT with the columns in a scrambled
/// order and verifies `FromPgRow::from_pg_row` panics via
/// `catch_unwind` (the attribute-macro `#[djogi_test]` does not
/// forward `#[should_panic]` because the user attribute is dropped
/// at expansion; in-body `catch_unwind` is the portable alternative).
///
/// Release builds skip `debug_assert!`; gate the test on
/// `cfg(debug_assertions)` so the workspace test run (debug) pins the
/// guard while a release build does not regress.
#[cfg(debug_assertions)]
#[djogi::djogi_test]
async fn from_pg_row_panics_on_drifted_column_order(mut ctx: djogi::DjogiContext) {
    setup_probe_table(&mut ctx).await;

    let created = T3Probe::create(
        &mut ctx,
        T3Probe {
            label: "panic".into(),
            count: 1,
            flag: false,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    // Columns reversed — position 0 is `flag` in the wire order, but
    // the macro-emitted `from_pg_row` decodes position 0 as `id` and
    // asserts the column name is `"id"`. The assert fires first.
    let drifted_sql = format!(
        "SELECT flag, count, label, updated_at, created_at, id FROM t3_probes WHERE id = {}",
        created.id.as_i64(),
    );
    let row = ctx
        .__query_one_for_macros(&drifted_sql, &[])
        .await
        .expect("drifted-order SELECT itself should succeed");

    // Expect a panic from the debug_assert_eq! in the generated
    // `from_pg_row`. `catch_unwind` lets us assert the panic payload
    // without relying on `#[should_panic]` forwarding through the
    // attribute macro wrapper. `AssertUnwindSafe` is required because
    // `tokio_postgres::Row` is not `UnwindSafe` (it wraps an Arc-backed
    // Statement) — the row is only read, so the unwind-safe assertion
    // is sound here.
    let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = <T3Probe as FromPgRow>::from_pg_row(&row);
    }))
    .expect_err("from_pg_row should panic on drifted column order");

    let msg = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&'static str>()
                .map(|s| s.to_string())
        })
        .unwrap_or_default();
    assert!(
        msg.contains("position 0 expected"),
        "panic message should mention the drifted position, got: {msg}"
    );
}

// Hand-rolled DDL matching the canonical column order (id, created_at,
// updated_at, then user fields). Matches `COLUMN_LIST` exactly so the
// ordinal-decode contract holds.
async fn setup_probe_table(ctx: &mut djogi::DjogiContext) {
    ctx.__execute_for_macros(
        "CREATE TABLE IF NOT EXISTS t3_probes (
            id         BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at TIMESTAMPTZ NOT NULL    DEFAULT now(),
            label      TEXT        NOT NULL,
            count      INTEGER     NOT NULL,
            flag       BOOLEAN     NOT NULL
        )",
        &[],
    )
    .await
    .expect("setup DDL should succeed");
}
