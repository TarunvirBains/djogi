// Phase 8.5 Cluster 2 issue #190 — narrow/unsigned integer column support.
//
// Internal wrapper. The body exercises round-trip, catalog assertions, and
// OOB rejection for i8/u8/u16/u32/u64 model fields. OOB insertion tests use
// `raw_execute` to construct out-of-range values that the typed Rust surface
// cannot produce (e.g. SMALLINT value 256 for a u8 column), hence the
// bypass attribute.

#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#190): exercises type-derived CHECK constraints for
// narrow/unsigned integer columns (i8/u8/u16/u32/u64 widened to
// SMALLINT/INTEGER/BIGINT/NUMERIC). OOB values are unreachable through
// the typed surface (e.g. SMALLINT value 256 for a u8 field). Raw INSERT
// SQL is the only way to construct these values to verify the CHECK fires.
// The round-trip and catalog assertion tests do NOT use raw_execute; only
// the five OOB rejection tests and the catalog constraint query do.
mod phase8_5_c2_190_integer_widening {
    include!("sources/phase8_5_c2_190_integer_widening.rs");
}
