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
//! | `relation`   | Relation field types — `ForeignKey<T>`, `OneToOneField<T>`, resolved-cache wrappers, and `OnDelete`. Landed in Phase 3 Task 1; extended by Tasks 2–6 with `RelationPath`, `ManyToMany`, and the macro glue. |
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
pub(crate) mod ident;
pub mod model;
pub mod query;
pub mod raw;
pub mod relation;
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

    /// Reflexive type-equality witness. Implemented for every `T` as
    /// `T: SameAs<T>`, so the **only** way for `A: SameAs<B>` to hold is
    /// `A == B`. Used by `{Model}Filter` setters to pin a method's value
    /// generic to the column's declared Rust type while keeping the
    /// `IntoFilterValue` bound deferrable (see the setter emission in
    /// `djogi-macros/src/model/filter.rs`). Not intended for downstream
    /// use — this lives in `__private` and carries no stability
    /// guarantee.
    pub trait SameAs<T: ?Sized> {}
    impl<T: ?Sized> SameAs<T> for T {}
}

pub use descriptor::{
    FieldDescriptor, FieldSqlType, IndexSpec, IndexType, ModelDescriptor, PartitionSpec, PkType,
};
pub use djogi_macros::{reverse_one_to_many, reverse_one_to_one};
pub use error::DjogiError;
pub use query::{
    Condition, FieldRef, FilterClause, IntoFilterValue, Lookup, ModelFilter, OrderExpr, QuerySet,
    UpdateAssignment, UpdateStmt,
};
pub use relation::{
    ForeignKey, ForeignKeyResolved, FromJoinedRow, JoinedRow, OnDelete, OneToOneField,
    OneToOneFieldResolved, PrefetchedRow,
};
pub use types::{Date, DateTime, HeerId, RanjId};

pub mod prelude {
    pub use crate::descriptor::{
        FieldDescriptor, FieldSqlType, IndexSpec, IndexType, ModelDescriptor, PartitionSpec, PkType,
    };
    pub use crate::error::DjogiError;
    pub use crate::model::Model;
    pub use crate::query::{
        Condition, FieldRef, FilterClause, IntoFilterValue, Lookup, ModelFilter, OrderExpr,
        QuerySet,
    };
    // Relation wrappers — unresolved (`ForeignKey`, `OneToOneField`) are
    // what user model structs declare; resolved (`ForeignKeyResolved`,
    // `OneToOneFieldResolved`) are what prefetched view structs receive,
    // and `OnDelete` is used at the `#[field(on_delete = ...)]` site. All
    // five belong in the prelude because a model defining any relation
    // needs the unresolved wrapper, and any handler consuming a
    // prefetched row needs the resolved wrapper.
    pub use crate::relation::{
        ForeignKey, ForeignKeyResolved, FromJoinedRow, JoinedRow, OnDelete, OneToOneField,
        OneToOneFieldResolved, PrefetchedRow,
    };
    pub use crate::types::{Date, DateTime, HeerId, RanjId};
    // Re-export the `#[model]` attribute macro so that `use djogi::prelude::*`
    // is the only import a model definition needs.
    pub use djogi_macros::model;
}
