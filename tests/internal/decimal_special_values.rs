// G0 — Decimal representability CHECK rejects PostgreSQL
// NUMERIC special values (`NaN`, `Infinity`, `-Infinity`) across the
// scalar Decimal, NUMRANGE endpoint, and NUMERIC[] helper surfaces.
//
// Internal wrapper. The OOB-rejection cases hand-craft PostgreSQL
// NUMERIC literals (`'NaN'`, `'Infinity'`, `'-Infinity'`) that
// `rust_decimal::Decimal` cannot construct — the type carries no
// non-finite states — so the only way to exercise the special-value
// rejection path is to drive raw SQL through `raw_execute`. Adopter-
// shaped integration tests stay raw-free per CLAUDE.md.

#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#188): exercises the `scale(<col>) IS NOT NULL`
// special-value guard added to `decimal_repr_expr` and the parallel
// `pg_catalog.scale(value) IS NOT NULL` clause in the
// `__djogi_numeric_array_is_rust_decimal_v1` helper. The three special
// NUMERIC values (`NaN`, `+Infinity`, `-Infinity`) are unreachable
// through `rust_decimal::Decimal` — the typed Rust path has no
// constructor for them — so the test crafts raw NUMERIC literals to
// verify the DB-level CHECK rejects each special value across scalar,
// NUMRANGE, and NUMERIC[] columns. The finite-value accept-path tests
// in the same fixture are intentionally typed-surface only; the
// `raw_execute` bypass is scoped to the rejection cases.
mod decimal_special_values {
    include!("sources/decimal_special_values.rs");
}
