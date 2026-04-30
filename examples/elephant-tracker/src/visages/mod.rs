//! Visages — projected views of `Herd`.
//!
//! The `HerdSummary` visage demonstrates the **side-query trait** pattern:
//! when an aggregate (here, "how many elephants are in this herd?") is
//! too expensive to denormalize onto a row but cheap to compute on demand,
//! the visage exposes it through a trait method that runs a separate
//! query rather than embedding the count in the projection's columns.

pub mod herd_summary;

pub use herd_summary::{HerdSummary, HerdSizeQuery};
