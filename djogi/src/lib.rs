//! Djogi — A Model-first web framework for Rust.
//!
//! Define your data schema as Rust structs, and the framework derives
//! everything else: ORM, migrations, admin UI, audit trail, shell bindings,
//! JSONB schema handling.
//!
//! Djogi's core is web-framework-agnostic — it owns the data layer and
//! delegates HTTP routing/middleware/rendering to whichever Rust web
//! framework the adopter chooses. Axum is the most-supported integration
//! target today and ships behind the opt-in `axum` feature flag; it is not
//! a core dependency.
//!
//! # Crate layout at a glance
//!
//! | Module       | Role |
//! |--------------|------|
//! | `config`     | `DjogiConfig` loaded from `Djogi.toml` + env (figment). |
//! | `context`    | `DjogiContext` — carries either a pooled handle or active transaction. Replaces `E: Executor` generics on `Model` + `QuerySet` signatures (Phase 4 Task 1). |
//! | `descriptor` | `ModelDescriptor` and friends — the single source of truth about every registered model. Populated by `#[model]` via `inventory::submit!`. |
//! | `error`      | `DjogiError` — the one error type returned by every `Model` method. |
//! | `model`      | The `Model` trait the macro implements for every user struct. Defined in Phase 1 Task 2. |
//! | `query`      | Filter AST: public API is `Condition` + `FieldRef` (plus `QuerySet<T>` and `OrderExpr`). Low-level enums (`Leaf`, `LookupOp`, `FilterValue`) live under `djogi::query::internal` for advanced/custom emitters. Filled in across Phase 2. |
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
pub mod context;
pub mod descriptor;
pub mod error;
pub mod expr;
pub(crate) mod ident;
pub mod model;
pub mod outbox;
pub mod pg;
pub mod projection;
pub mod query;
pub mod relation;
pub mod testing;
pub mod transaction;
pub mod types;

/// Private re-exports used only by macro-generated code.
///
/// These are `#[doc(hidden)]` because they are an implementation detail of the
/// `#[model]` macro, not part of the public API. Macro-emitted code uses fully
/// qualified paths like `::djogi::__private::inventory::submit!` so that users
/// only need `djogi` as a direct dependency — they never need to add
/// `inventory` or `time` themselves.
///
/// T2 adds `::djogi::__private::pg` containing the new SQL substrate types
/// (`SqlAccumulator`, `PgConnection`, `ToSql`, `FromSql`, `PgRow`). Macro-
/// emitted code routes through `::djogi::__private::pg::*` rather than
/// importing `tokio_postgres` / `postgres_types` directly.
#[doc(hidden)]
pub mod __private {
    pub use futures;
    pub use inventory;
    pub use postgres_types;
    pub use serde;
    pub use tokio_postgres;

    /// New SQL substrate re-exports for macro-emitted code.
    ///
    /// Macro emission routes through `::djogi::__private::pg::*` rather than
    /// directly importing `tokio_postgres::*` or `postgres_types::*` — this
    /// keeps the macro output decoupled from the exact crate versions and
    /// allows Djogi to add wrapper types without changing the macro-emitted
    /// call sites. See `feedback_macro_path_routing.md` for the rationale.
    pub mod pg {
        pub use crate::pg::accumulator::SqlAccumulator;
        pub use crate::pg::connection::PgConnection;
        /// Canonical row-decode trait (T3). Emitted by `#[model]` with
        /// `const COLUMNS`, `const COLUMN_LIST`, and an ordinal
        /// `from_pg_row` body guarded by per-column `debug_assert!`s.
        pub use crate::pg::decode::{
            FromJoinedPgRow, FromPgRow, FromRowTuple, try_get_scalar, try_get_tuple,
        };
        pub use ::postgres_types::{FromSql, ToSql, Type as PgType};
        pub use ::tokio_postgres::Row as PgRow;
        pub use ::tokio_postgres::Statement;
    }

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

pub use context::DjogiContext;
pub use descriptor::{
    FieldDescriptor, FieldSqlType, IndexSpec, IndexType, ModelDescriptor, PartitionSpec, PkType,
};
pub use djogi_macros::{many_to_many, reverse_one_to_many, reverse_one_to_one};
pub use pg::decode::{FromJoinedPgRow, FromPgRow, FromRowTuple, try_get_scalar, try_get_tuple};
// The `#[djogi_test]` attribute macro re-exported for convenience. The macro
// itself is always available (proc macros have no runtime component); the
// *runtime helper* it calls (`::djogi::testing::setup_test_db`) is gated on
// `cfg(any(test, feature = "testing"))` so the generated code only compiles
// in test or feature-enabled builds.
pub use djogi_macros::djogi_test;
pub use error::{DbError, DjogiError};
pub use expr::{AggregateExpr, Case, CaseBuilder, Exists, Expr, OuterRef, Subquery};
pub use projection::ProjectionError;
pub use query::{
    AggregateQuery, AnnotatedQuerySet, Condition, FieldRef, FilterClause, IntoAggregateTuple,
    IntoFilterValue, Lookup, ModelFilter, OrderExpr, QuerySet, UpdateAssignment, UpdateStmt,
};
pub use relation::{
    ForeignKey, ForeignKeyResolved, JoinedRow, ManyToMany, OnDelete, OneToOneField,
    OneToOneFieldResolved, PrefetchedRow,
};
pub use types::{Date, DateTime, HeerId, RanjId};

pub mod prelude {
    pub use crate::context::DjogiContext;
    pub use crate::descriptor::{
        FieldDescriptor, FieldSqlType, IndexSpec, IndexType, ModelDescriptor, PartitionSpec, PkType,
    };
    pub use crate::error::{DbError, DjogiError};
    pub use crate::expr::{AggregateExpr, Case, CaseBuilder, Exists, Expr, OuterRef, Subquery};
    pub use crate::model::Model;
    pub use crate::pg::decode::{
        FromJoinedPgRow, FromPgRow, FromRowTuple, try_get_scalar, try_get_tuple,
    };
    pub use crate::projection::ProjectionError;
    pub use crate::query::{
        AggregateQuery, AnnotatedQuerySet, Condition, FieldRef, FilterClause, IntoAggregateTuple,
        IntoFilterValue, Lookup, ModelFilter, OrderExpr, QuerySet,
    };
    // `atomic` / `retry_on_conflict` — Phase 4 Task 1 canonical
    // transaction scope + retry helper.
    pub use crate::transaction::{atomic, retry_on_conflict};
    // Relation wrappers — unresolved (`ForeignKey`, `OneToOneField`) are
    // what user model structs declare; resolved (`ForeignKeyResolved`,
    // `OneToOneFieldResolved`) are what prefetched view structs receive,
    // and `OnDelete` is used at the `#[field(on_delete = ...)]` site. All
    // five belong in the prelude because a model defining any relation
    // needs the unresolved wrapper, and any handler consuming a
    // prefetched row needs the resolved wrapper.
    pub use crate::relation::{
        ForeignKey, ForeignKeyResolved, JoinedRow, ManyToMany, OnDelete, OneToOneField,
        OneToOneFieldResolved, PrefetchedRow,
    };
    pub use crate::types::{Date, DateTime, HeerId, RanjId};
    // Re-export the `#[model]` attribute macro so that `use djogi::prelude::*`
    // is the only import a model definition needs.
    pub use djogi_macros::model;
    // Re-export the `#[djogi_test]` attribute macro for test functions.
    // The macro generates code that calls `::djogi::testing::setup_test_db`;
    // use only in test binaries.
    pub use djogi_macros::djogi_test;
}
