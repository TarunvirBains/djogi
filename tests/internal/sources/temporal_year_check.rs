// Issue #187 — temporal year-bounds CHECK projection.
//
// # What this file pins
//
// The type-derived CHECK projection contract (`docs/spec/migrations.md`
// §10.6.1) requires `time::Date` and `time::OffsetDateTime` columns to
// carry a year upper-bound CHECK so external writers cannot land
// OOB-upper-year rows that would later fail typed `SELECT` decode with
// `DjogiError::Decode`. The bound is one-sided by design — Postgres's
// own date input parser rejects every value `time::Date` cannot
// represent on the lower end (Postgres's MIN is 4713 BC; `time::Date`'s
// MIN is 10000 BC, unreachable through Postgres regardless of CHECK).
//
// Three behaviours are pinned:
//
// 1. **Round-trip with the CHECK on.** A typed `Model::create` of a row
//    whose `Date` / `OffsetDateTime` values fall within ±9999 succeeds
//    end-to-end through `sync_models → INSERT → RETURNING → FromPgRow`.
// 2. **OOB-upper Date rejection.** A raw `INSERT` with year > 9999 (here:
//    year 12000) is rejected by Postgres at write time with a constraint
//    violation referencing the table+column CHECK constraint name
//    (`{table}_{column}_check`).
// 3. **OOB-upper Timestamptz rejection.** Same shape on the timestamp
//    column.
//
// # Why a tests/internal target
//
// The OOB-rejection assertions construct Postgres values that are
// unreachable through the `time` crate's default API (`time::Date`
// caps at ±9999 without the `large-dates` feature, which djogi does
// NOT enable). The only way to exercise OOB-upper write rejection is
// to hand-craft the SQL literal via `raw_execute`. Tests that reach
// for raw access live under `tests/internal/`; adopter-shaped
// integration tests stay raw-free per CLAUDE.md.
//
// # Spec anchors
//
// - GH #187 — Auto-emit range CHECK for temporal columns
//   (Date / OffsetDateTime / PrimitiveDateTime)
// - `docs/spec/migrations.md` §10.6.1 — Type-Derived CHECK Projection
//   contract; temporal arms are the first live family.
// - `djogi/src/migrate/projection.rs::field_type_check` — Date /
//   Timestamptz arms emit the year-bound expression.

use djogi::prelude::*;

#[model(table = "c2_187_temporal_rows", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct C2187TemporalRow {
    pub event_on: ::time::Date,
    pub recorded_at: ::time::OffsetDateTime,
    pub label: String,
}

#[djogi::djogi_test(sync_models = [C2187TemporalRow])]
async fn temporal_year_check_in_range_round_trip(mut ctx: djogi::DjogiContext) {
    // ── Behaviour 1: valid year round-trips end-to-end ───────────────────
    //
    // Year 2026 is well inside ±9999 — CHECK passes, INSERT succeeds,
    // and the typed `Model::create` returns the persisted row.
    let row = C2187TemporalRow::create(
        &mut ctx,
        C2187TemporalRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            event_on: ::time::Date::from_calendar_date(2026, ::time::Month::May, 15).unwrap(),
            recorded_at: ::time::OffsetDateTime::from_unix_timestamp(1_747_400_000).unwrap(),
            label: "djogi#187 in-range".into(),
        },
    )
    .await
    .expect("in-range Date/OffsetDateTime values must round-trip");

    // Sanity: refetch via Model::get exercises the FromPgRow decode path
    // which would otherwise surface OOB-year poisoning as DjogiError::Decode.
    let fetched = C2187TemporalRow::get(&mut ctx, row.id)
        .await
        .expect("get should round-trip");
    assert_eq!(fetched.label, "djogi#187 in-range");
    assert_eq!(
        fetched.event_on,
        ::time::Date::from_calendar_date(2026, ::time::Month::May, 15).unwrap()
    );
}

#[djogi::djogi_test(sync_models = [C2187TemporalRow])]
async fn temporal_year_check_rejects_oob_date(mut ctx: djogi::DjogiContext) {
    // ── Behaviour 2: year > 9999 on Date column is rejected ──────────────
    //
    // We hand-craft a raw INSERT bound with a Postgres DATE literal at
    // year 12000. Postgres accepts the literal as a valid DATE value
    // (its native DATE range goes up to 5874897 AD), so without the
    // type-derived CHECK the INSERT would succeed and the row would
    // silently land in the table — corrupting subsequent typed reads
    // through `FromPgRow` because `time::Date` cannot represent year
    // 12000 in its default ±9999 range.
    //
    // With the CHECK in place, Postgres rejects the INSERT at write
    // time with a `check constraint
    // "c2_187_temporal_rows_event_on_check"` violation. The
    // constraint name comes from
    // `migrate::sql::check_constraint_name(table, column)`, emitted
    // inline on CREATE TABLE via the `CONSTRAINT <name> CHECK (...)`
    // form so the name is deterministic (Postgres's auto-naming for
    // unnamed inline CHECKs uses `{table}_check` / `{table}_check1` —
    // inconsistent with the differ's ALTER TABLE DROP CONSTRAINT
    // path).
    let err = ctx
        .raw_execute(
            "INSERT INTO c2_187_temporal_rows \
             (event_on, recorded_at, label) \
             VALUES (DATE '12000-01-01', now(), 'oob date')",
            &[],
        )
        .await
        .expect_err("OOB-upper-year Date INSERT must be rejected by the type-derived CHECK");

    let msg = format!("{err:?}");
    // Postgres surfaces the check constraint violation. The error
    // message must reference the constraint name our projection emits;
    // otherwise either the CHECK is missing or the naming convention
    // drifted from `migrate::sql::check_constraint_name`.
    assert!(
        msg.contains("c2_187_temporal_rows_event_on_check"),
        "OOB Date INSERT error must reference the type-derived CHECK \
         constraint name (got: {msg})"
    );
}

#[djogi::djogi_test(sync_models = [C2187TemporalRow])]
async fn temporal_year_check_rejects_oob_timestamptz(mut ctx: djogi::DjogiContext) {
    // ── Behaviour 3: year > 9999 on Timestamptz column is rejected ───────
    //
    // Same shape as the Date OOB test but on the OffsetDateTime
    // column with year 12000 (one above the +9999 upper bound).
    // Postgres's native TIMESTAMPTZ range extends to 294276 AD, so
    // year 12000 is a valid Postgres literal without the CHECK.
    let err = ctx
        .raw_execute(
            "INSERT INTO c2_187_temporal_rows \
             (event_on, recorded_at, label) \
             VALUES (DATE '2026-05-15', TIMESTAMP '12000-01-01 00:00:00', 'oob ts')",
            &[],
        )
        .await
        .expect_err(
            "OOB-upper-year OffsetDateTime INSERT must be rejected by the type-derived CHECK",
        );

    let msg = format!("{err:?}");
    assert!(
        msg.contains("c2_187_temporal_rows_recorded_at_check"),
        "OOB Timestamptz INSERT error must reference the type-derived CHECK \
         constraint name (got: {msg})"
    );
}

#[djogi::djogi_test(sync_models = [C2187TemporalRow])]
async fn timestamptz_check_is_utc_invariant_under_non_utc_session_timezone(
    mut ctx: djogi::DjogiContext,
) {
    // ── Behaviour 4: TIMESTAMPTZ CHECK is UTC-explicit, not session-tz-sensitive ─
    //
    // Regression guard for the djogi#187 fix: the projected CHECK uses
    // `TIMESTAMPTZ '9999-12-31 23:59:59.999999+00'` (UTC-explicit), NOT the
    // old `TIMESTAMP '...'` (plain, session-timezone-interpreted) form.
    //
    // With the broken form, a session running in UTC-5 (e.g. America/New_York
    // during standard time) would interpret the CHECK literal in local time,
    // widening the effective UTC upper bound by 5 hours — values up to
    // `10000-01-01 04:59:59 UTC` would pass the broken CHECK. Year 10000 AD
    // is within Postgres's native TIMESTAMPTZ range (which extends to 294276 AD),
    // so such a row would land and corrupt subsequent typed reads.
    //
    // With the correct UTC-explicit form, a value of `10000-01-01 00:00:00+00`
    // (just over the UTC bound) is rejected regardless of the session timezone.
    //
    // Test strategy:
    //   1. SET LOCAL timezone to UTC-5 (`Etc/GMT+5`) for the current transaction.
    //   2. INSERT `TIMESTAMPTZ '10000-01-01 00:00:00+00'` (4 hours 59 min 59 s
    //      below the broken UTC-5 local bound, but above the correct UTC bound).
    //   3. Expect a CHECK violation naming `recorded_at_check`.
    ctx.raw_execute("SET LOCAL timezone = 'Etc/GMT+5'", &[])
        .await
        .expect("SET LOCAL timezone must succeed in a transaction context");

    let err = ctx
        .raw_execute(
            "INSERT INTO c2_187_temporal_rows \
             (event_on, recorded_at, label) \
             VALUES (DATE '2026-05-15', TIMESTAMPTZ '10000-01-01 00:00:00+00', 'tz-boundary')",
            &[],
        )
        .await
        .expect_err(
            "TIMESTAMPTZ value '10000-01-01 00:00:00+00' must be rejected by the UTC-explicit \
             CHECK even when the session timezone is UTC-5 (Etc/GMT+5)",
        );

    let msg = format!("{err:?}");
    assert!(
        msg.contains("c2_187_temporal_rows_recorded_at_check"),
        "OOB Timestamptz INSERT under non-UTC session timezone must reference the \
         type-derived CHECK constraint name (got: {msg})"
    );
}
