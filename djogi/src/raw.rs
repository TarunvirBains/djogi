//! Raw SQL escape hatch — fully implemented in Phase 1 Task 11.
//!
//! The public API (`djogi::raw::query`, `query_as`, `query_scalar`, `execute`)
//! mirrors `sqlx`'s positional-arg constructors but defaults to Postgres and
//! returns `DjogiError` on decode failures. Task 11 wires all of that up; for
//! Task 1 the module just needs to exist so `djogi::raw` resolves.
