// Cluster 2 issues #105 + #188 — adopter `#[field(check)]` and
// Decimal structural CHECK projection.
//
// Internal wrapper. The OOB-rejection assertions construct Postgres values
// that cannot be produced through `rust_decimal::Decimal` (the typed Rust
// path caps at the rust_decimal representable range; the structural CHECK
// is precisely there to catch values written by external writers that fall
// outside that range). Hand-crafting `INSERT … NUMERIC '99999...'` via raw
// SQL is the only way to test the rejection path.

#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#188): exercises the type-derived structural CHECK on
// `rust_decimal::Decimal` columns. Out-of-range values (100-digit integers,
// scale > 28) are unreachable through the typed Rust surface — rust_decimal
// itself rejects them — so the test must craft raw NUMERIC literals via
// `raw_execute` to verify the DB-level CHECK fires. The round-trip and
// `#[field(check)]` (djogi#105) typed-surface tests do NOT use raw_execute;
// only the OOB rejection tests and the catalog constraint query do.
mod c2_105_188_check_decimal {
    include!("sources/c2_105_188_check_decimal.rs");
}
