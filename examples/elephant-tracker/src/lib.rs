//! Library face of `elephant-tracker` — exposes the models and the
//! seed / migrate / demo modules so integration tests in `tests/` can
//! reach them through `use elephant_tracker::...` (the bin target's
//! private `mod models;` view is unreachable from tests).
//!
//! # Why both `main.rs` and `lib.rs`
//!
//! The binary target (`main.rs`) ships the `elephant-tracker` CLI;
//! its modules (`models`, `seed`, `migrate`, `demos`, `output`,
//! `visages`) are private to the binary. Cargo's default integration-
//! test target sees only the library half of the crate, so tests
//! under `tests/` that need `Elephant`, `ElephantAncestry`, `Herd`,
//! etc. would otherwise have to inline the model definitions —
//! duplication that drifts as the schema evolves.
//!
//! Both targets compile the same module sources. The lib target
//! exposes them publicly; the bin target imports them privately via
//! `mod ...` declarations in `main.rs`. Rust's compile model handles
//! both compilations independently — there is no double-link or
//! diamond-dependency problem because the two targets are different
//! crates from Cargo's perspective.

pub mod demos;
pub mod migrate;
pub mod models;
pub mod output;
pub mod seed;
pub mod visages;
