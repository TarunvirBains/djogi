// Phase 8.5 G0 — Temporal representability CHECK rejects PostgreSQL
// DATE / TIMESTAMPTZ special values (`+infinity`, `-infinity`) across
// the scalar Date / Timestamptz, the DATERANGE / TSTZRANGE endpoint,
// and the DATE[] / TIMESTAMPTZ[] array-element surfaces.
//
// Internal wrapper. The OOB-rejection cases hand-craft PostgreSQL
// DATE / TIMESTAMPTZ literals (`'-infinity'::date`, `'infinity'`,
// `daterange('-infinity', X)`, `ARRAY['-infinity'::date]`) that
// `time::Date`, `time::OffsetDateTime`, and `Vec<time::Date>` /
// `Vec<time::OffsetDateTime>` cannot construct — none of these types
// carry a non-finite state — so the only way to exercise the
// special-value rejection path is to drive raw SQL through
// `raw_execute`. Adopter-shaped integration tests stay raw-free per
// CLAUDE.md.

#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#187): exercises the `pg_catalog.isfinite(<col>)`
// / `djogi.__djogi_date_array_is_finite_v1` /
// `djogi.__djogi_tstz_array_is_finite_v1` finite-value guards across all
// five projection surfaces: scalar Date, scalar Timestamptz, DATERANGE
// endpoints, TSTZRANGE endpoints, DATE[] elements, and TIMESTAMPTZ[]
// elements. The two non-finite special values (`+infinity`, `-infinity`)
// are unreachable through `time::Date` / `time::OffsetDateTime` or their
// `Vec<_>` wrappers — neither type carries a non-finite state — so the
// test crafts raw temporal literals to verify the DB-level CHECK rejects
// each special value. The finite-value accept-path tests and empty-array
// pass-through tests in the same fixture are intentionally typed-surface
// only; the `raw_execute` bypass is scoped to the rejection cases.
mod phase8_5_g0_temporal_infinity {
    include!("sources/phase8_5_g0_temporal_infinity.rs");
}
