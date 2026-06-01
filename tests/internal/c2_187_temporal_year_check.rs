// Cluster 2 issue #187 — temporal year-bounds CHECK projection.
//
// Internal wrapper. The body issues `raw_execute` against hand-crafted
// SQL literals (`DATE '12000-01-01'`, `TIMESTAMP '-10001-01-01 00:00:00'`)
// that the `time` crate's default API cannot construct — `time::Date`
// caps at ±9999 without the `large-dates` feature, which djogi does
// NOT enable. The only way to exercise OOB write rejection is through
// raw SQL, so this fixture lives under `tests/internal/` per the
// CLAUDE.md rule that adopter-shaped integration tests stay raw-free.

#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#187): exercises the type-derived CHECK projection
// for `time::Date` and `time::OffsetDateTime` columns. OOB-year values
// are unreachable through the typed surface (the `time` crate's default
// year range is ±9999); we hand-craft raw INSERTs with Postgres DATE /
// TIMESTAMP literals at year 12000 / -10001 / -10000 to prove the CHECK
// rejects writes that would otherwise corrupt typed reads via
// `DjogiError::Decode`.
mod c2_187_temporal_year_check {
    include!("sources/c2_187_temporal_year_check.rs");
}
