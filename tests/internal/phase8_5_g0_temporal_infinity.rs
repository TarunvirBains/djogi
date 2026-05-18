// Phase 8.5 G0 — Temporal representability CHECK rejects PostgreSQL
// DATE / TIMESTAMPTZ special values (`+infinity`, `-infinity`) across
// the scalar Date / Timestamptz and the DATERANGE / TSTZRANGE endpoint
// surfaces.
//
// Internal wrapper. The OOB-rejection cases hand-craft PostgreSQL
// DATE / TIMESTAMPTZ literals (`'-infinity'::date`, `'infinity'`,
// `daterange('-infinity', X)`) that `time::Date` and
// `time::OffsetDateTime` cannot construct — neither type carries a
// non-finite state — so the only way to exercise the special-value
// rejection path is to drive raw SQL through `raw_execute`. Adopter-
// shaped integration tests stay raw-free per CLAUDE.md.

#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#187): exercises the `pg_catalog.isfinite(<col>)`
// finite-value guard added to `date_range_expr` /
// `timestamptz_range_expr` for the scalar Date / Timestamptz CHECK and,
// by reuse through `range_endpoint_checks`, for the DATERANGE /
// TSTZRANGE endpoint CHECK. The two non-finite special values
// (`+infinity`, `-infinity`) are unreachable through `time::Date` /
// `time::OffsetDateTime` — neither type carries a non-finite state —
// so the test crafts raw temporal literals to verify the DB-level
// CHECK rejects each special value across scalar, DATERANGE, and
// TSTZRANGE columns. The finite-value accept-path tests in the same
// fixture are intentionally typed-surface only; the `raw_execute`
// bypass is scoped to the rejection cases.
mod phase8_5_g0_temporal_infinity {
    include!("sources/phase8_5_g0_temporal_infinity.rs");
}
