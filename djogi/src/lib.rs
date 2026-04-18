//! Djogi — A Model-first web framework for Rust, built on Axum.
//!
//! Define your data schema as Rust structs, and the framework derives
//! everything else: ORM, migrations, admin UI, audit trail, shell bindings,
//! JSONB schema handling.
//!
//! # Crate layout at a glance
//!
//! | Module       | Role |
//! |--------------|------|
//! | `config`     | `DjogiConfig` loaded from `Djogi.toml` + env (figment). |
//! | `descriptor` | `ModelDescriptor` and friends — the single source of truth about every registered model. Populated by `#[model]` via `inventory::submit!`. |
//! | `error`      | `DjogiError` — the one error type returned by every `Model` method. |
//! | `model`      | The `Model` trait the macro implements for every user struct. Defined in Phase 1 Task 2. |
//! | `query`      | Filter AST: public API is `Condition` + `FieldRef` (plus `QuerySet<T>` and `OrderExpr`). Low-level enums (`Leaf`, `LookupOp`, `FilterValue`) live under `djogi::query::internal` for advanced/custom emitters. Filled in across Phase 2. |
//! | `raw`        | `djogi::raw::*` escape hatches for when `QuerySet` is too limiting. Fully implemented in Phase 1 Task 11. |
//! | `types`      | `DateTime`, `Date`, and re-exports of `HeerId`/`RanjId` — the canonical types imported via `prelude`. |
//!
//! # Recommended usage
//!
//! ```ignore
//! use djogi::prelude::*;
//!
//! #[model(table = "posts")]
//! struct Post {
//!     title: String,
//!     body: String,
//! }
//! ```
//!
//! `prelude::*` brings in the `#[model]` attribute macro, `Model` trait,
//! canonical types (`DateTime`, `Date`, `HeerId`, `RanjId`), and the
//! `DjogiError` enum — everything a model definition needs.

pub mod config;
pub mod descriptor;
pub mod error;
pub mod model;
pub mod query;
pub mod raw;
pub mod types;

/// Private re-exports used only by macro-generated code.
///
/// These are `#[doc(hidden)]` because they are an implementation detail of the
/// `#[model]` macro, not part of the public API. Macro-emitted code uses fully
/// qualified paths like `::djogi::__private::sqlx::FromRow` and
/// `::djogi::__private::inventory::submit!` so that users only need `djogi` as
/// a direct dependency — they never need to add `sqlx`, `inventory`, or
/// `time` themselves.
#[doc(hidden)]
pub mod __private {
    pub use inventory;
    pub use sqlx;
}

pub use descriptor::{
    FieldDescriptor, FieldSqlType, IndexSpec, IndexType, ModelDescriptor, PartitionSpec, PkType,
};
pub use error::DjogiError;
pub use query::{Condition, FieldRef, IntoFilterValue, OrderExpr, QuerySet};
pub use types::{Date, DateTime, HeerId, RanjId};

pub mod prelude {
    pub use crate::descriptor::{
        FieldDescriptor, FieldSqlType, IndexSpec, IndexType, ModelDescriptor, PartitionSpec, PkType,
    };
    pub use crate::error::DjogiError;
    pub use crate::model::Model;
    pub use crate::query::{Condition, FieldRef, IntoFilterValue, OrderExpr, QuerySet};
    pub use crate::types::{Date, DateTime, HeerId, RanjId};
    // Re-export the `#[model]` attribute macro so that `use djogi::prelude::*`
    // is the only import a model definition needs.
    pub use djogi_macros::model;
}
