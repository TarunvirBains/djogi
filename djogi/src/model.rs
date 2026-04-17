//! The `Model` trait — fully defined in Phase 1 Task 2.
//!
//! This stub carries only the trait skeleton so `djogi::prelude::Model`
//! resolves during Task 1's compile check. Task 2 replaces it with the
//! full trait (associated `Pk` type, `get`/`create`/`save`/`delete`/
//! `refresh` futures, connection-generic executor bounds). The macro
//! cannot implement anything from this stub yet — no CRUD exists here.

/// Placeholder — see Task 2 for the full trait.
pub trait Model: Sized + Send + Sync + 'static {}
