//! Djogi — A Model-first web framework for Rust, built on Axum.
//!
//! Define your data schema as Rust structs, and the framework derives
//! everything else: ORM, migrations, admin UI, audit trail, shell bindings,
//! JSONB schema handling.

pub mod config;

pub use djogi_macros::*;

pub mod prelude {
    pub use crate::*;
}
