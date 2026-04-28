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

pub mod apps;
pub mod array;
pub mod auth;
pub mod config;
pub mod context;
pub mod descriptor;
pub mod enum_;
pub mod error;
pub mod expr;
pub mod fts;
pub mod fts_query;
#[cfg(feature = "spatial")]
pub mod geo;
pub(crate) mod ident;
pub mod jsonb;
pub mod model;
pub mod outbox;
pub mod pg;
pub mod primary_key;
pub mod query;
pub mod relation;
pub mod testing;
pub mod tracked;
pub mod transaction;
pub mod types;
pub mod visage;
pub mod visage_boundary;

// T7 fixup — re-export `DjogiVisageOf` at crate root so adopter code that
// bounds generics on "something that projects model M" can spell the
// trait as `djogi::DjogiVisageOf<M>` rather than reaching into the
// internal `visage_boundary` module. The trait itself is stable public
// API; only the module it lives in is implementation-detail.
pub use visage_boundary::DjogiVisageOf;

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
    pub use bytes;

    pub mod pg {
        pub use crate::pg::accumulator::SqlAccumulator;
        pub use crate::pg::connection::PgConnection;
        /// Canonical row-decode trait (T3). Emitted by `#[model]` with
        /// `const COLUMNS`, `const COLUMN_LIST`, and an ordinal
        /// `from_pg_row` body guarded by per-column `debug_assert!`s.
        pub use crate::pg::decode::{
            FromJoinedPgRow, FromPgRow, FromRowTuple, decode_at, try_get_scalar, try_get_tuple,
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

    /// Visage boundary marker + its seal.
    ///
    /// Proc-macro-emitted visages impl `Sealed<M>` and `DjogiVisageOf<M>`
    /// for their source model `M`. Downstream code cannot satisfy the
    /// sealed supertrait, so no hostile `impl DjogiVisageOf<OtherModel>`
    /// can slip in. The re-export through `__private` keeps the macro
    /// output routed through `::djogi::*` paths per
    /// `feedback_macro_path_routing.md`.
    pub use crate::visage_boundary::DjogiVisageOf;
    pub use crate::visage_boundary::private::Sealed as VisageSealed;

    /// Hidden seal-token witnesses for [`crate::primary_key::PrimaryKey`]
    /// and [`crate::apps::App`].
    ///
    /// The public `djogi::primary_key` and `djogi::apps` paths used to
    /// re-export the token consts (`__DJOGI_PK_SEAL_TOKEN`,
    /// `__DJOGI_APPS_SEAL_TOKEN`). That made the seals bypassable —
    /// downstream code could grab the public consts and hand-roll a
    /// trait impl. The consts now live only here, under the
    /// `__private` namespace whose contract states "downstream code
    /// reaching in is breaking the framework boundary; we reserve the
    /// right to break that code in any future release without notice."
    /// Same convention as `VisageSealed` above.
    pub mod pk_seal {
        pub use crate::primary_key::PkSealToken;

        /// Sole [`PkSealToken`] value — reached from macro-emitted
        /// code via `::djogi::__private::pk_seal::TOKEN`.
        pub const TOKEN: PkSealToken = PkSealToken::__new();
    }
    pub mod apps_seal {
        pub use crate::apps::SealToken;

        /// Sole [`SealToken`] value — reached from macro-emitted code
        /// via `::djogi::__private::apps_seal::TOKEN`.
        pub const TOKEN: SealToken = SealToken::__new();
    }

    /// `tracing` re-export for macro-generated `_insecurely()` warn! calls.
    ///
    /// Routing through `::djogi::__private::tracing` keeps user crates from
    /// needing `tracing` as a direct dependency — the same path-routing
    /// convention used for `inventory`, `postgres_types`, and `futures`.
    pub use tracing;
}

pub use apps::{App, AppDescriptor, AppDiagnostic, AppIdentity, AppRegistry, CrossAppEdge};
pub use context::DjogiContext;
pub use descriptor::{
    EnumDescriptor, FieldDescriptor, FieldSqlType, GeographySubtype, IndexColumnSpec, IndexKind,
    IndexNameKind, IndexNameTarget, IndexNullsOrder, IndexOrder, IndexSpec, IndexTarget, IndexType,
    ModelDescriptor, PartitionSpec, PkType, index_name,
};
// Top-level `djogi::GeoPoint` re-export for spatial models. Feature-gated so
// the symbol does not appear in default-feature builds or `cargo doc` output
// when PostGIS support is not requested.
pub use djogi_macros::{
    DjogiEnum, JsonbSchema, apps, many_to_many, primary_key, reverse_one_to_many,
    reverse_one_to_one,
};
#[cfg(feature = "spatial")]
pub use geo::GeoPoint;
pub use jsonb::{Jsonb, JsonbPathRef, JsonbSchema, UnknownField, UnknownFieldExt};
pub use pg::decode::{FromJoinedPgRow, FromPgRow, FromRowTuple, try_get_scalar, try_get_tuple};
pub use primary_key::{PrimaryKey, PrimaryKeyClientGen, PrimaryKeyDbGen};
// The `#[djogi_test]` attribute macro re-exported for convenience. The macro
// itself is always available (proc macros have no runtime component); the
// *runtime helper* it calls (`::djogi::testing::setup_test_db`) is gated on
// `cfg(any(test, feature = "testing"))` so the generated code only compiles
// in test or feature-enabled builds.
pub use djogi_macros::djogi_test;
pub use error::{DbError, DjogiError};
pub use expr::{AggregateExpr, Case, CaseBuilder, Exists, Expr, OuterRef, Subquery};
pub use fts::{FtsDescriptor, TsQuery, TsVector};
pub use fts_query::FtsFieldRef;
pub use query::{
    AggregateQuery, AnnotatedQuerySet, Condition, FieldRef, FilterClause, IntoAggregateTuple,
    IntoFilterValue, Lookup, ModelCursorStream, ModelFilter, OrderExpr, QuerySet, RawCursorStream,
    UpdateAssignment, UpdateStmt, VisageQuerySet,
};
pub use relation::{
    ForeignKey, ForeignKeyResolved, JoinedRow, ManyToMany, OnDelete, OneToOneField,
    OneToOneFieldResolved, PrefetchedRow,
};
pub use tracked::Tracked;
pub use types::{
    Date, DateTime, HeerId, HeerIdDesc, HeerIdRecencyBiased, RanjId, RanjIdDesc,
    RanjIdRecencyBiased,
};
pub use visage::VisageError;

pub mod prelude {
    pub use crate::apps::{
        App, AppDescriptor, AppDiagnostic, AppIdentity, AppRegistry, CrossAppEdge,
    };
    pub use crate::context::DjogiContext;
    pub use crate::descriptor::{
        EnumDescriptor, FieldDescriptor, FieldSqlType, GeographySubtype, IndexColumnSpec,
        IndexKind, IndexNullsOrder, IndexOrder, IndexSpec, IndexTarget, IndexType, ModelDescriptor,
        PartitionSpec, PkType,
    };
    pub use crate::error::{DbError, DjogiError};
    pub use crate::expr::{AggregateExpr, Case, CaseBuilder, Exists, Expr, OuterRef, Subquery};
    pub use crate::fts::{FtsDescriptor, TsQuery, TsVector};
    pub use crate::fts_query::FtsFieldRef;
    pub use crate::jsonb::{Jsonb, JsonbPathRef, JsonbSchema, UnknownField, UnknownFieldExt};
    pub use crate::model::Model;
    pub use crate::pg::decode::{
        FromJoinedPgRow, FromPgRow, FromRowTuple, try_get_scalar, try_get_tuple,
    };
    pub use crate::query::{
        AggregateQuery, AnnotatedQuerySet, Condition, FieldRef, FilterClause, IntoAggregateTuple,
        IntoFilterValue, Lookup, ModelFilter, OrderExpr, QuerySet, VisageQuerySet,
    };
    // `atomic` / `retry_on_conflict` — Phase 4 Task 1 canonical
    // transaction scope + retry helper.
    pub use crate::transaction::{atomic, retry_on_conflict};
    pub use crate::visage::VisageError;
    // Relation wrappers — unresolved (`ForeignKey`, `OneToOneField`) are
    // what user model structs declare; resolved (`ForeignKeyResolved`,
    // `OneToOneFieldResolved`) are what prefetched view structs receive,
    // and `OnDelete` is used at the `#[field(on_delete = ...)]` site. All
    // five belong in the prelude because a model defining any relation
    // needs the unresolved wrapper, and any handler consuming a
    // prefetched row needs the resolved wrapper.
    pub use crate::primary_key::{PrimaryKey, PrimaryKeyClientGen, PrimaryKeyDbGen};
    pub use crate::relation::{
        ForeignKey, ForeignKeyResolved, JoinedRow, ManyToMany, OnDelete, OneToOneField,
        OneToOneFieldResolved, PrefetchedRow,
    };
    pub use crate::tracked::Tracked;
    pub use crate::types::{
        Date, DateTime, HeerId, HeerIdDesc, HeerIdRecencyBiased, RanjId, RanjIdDesc,
        RanjIdRecencyBiased,
    };
    // T7 fixup — `DjogiVisageOf<M>` is the seal trait bounding every
    // `{Visage}` type to its source model `M`. Adopter code that writes
    // generic bounds over "any projection of M" names this trait, so it
    // belongs in the default prelude alongside `Model`.
    pub use crate::visage_boundary::DjogiVisageOf;
    // Re-export the `#[model]` attribute macro so that `use djogi::prelude::*`
    // is the only import a model definition needs.
    pub use djogi_macros::model;
    // Re-export the `djogi::apps!` function-like macro — required to declare
    // compile-time schema ownership domains (Phase 7-Zero v3 T7).
    pub use djogi_macros::apps;
    // Re-export the `djogi::primary_key!` function-like macro — lets
    // adopters declare custom PK newtypes (Phase 7-Zero-2 T3).
    pub use djogi_macros::primary_key;
    // Re-export the `#[djogi_test]` attribute macro for test functions.
    // The macro generates code that calls `::djogi::testing::setup_test_db`;
    // use only in test binaries.
    pub use djogi_macros::djogi_test;
    // Re-export the `#[derive(DjogiEnum)]` derive macro.
    pub use djogi_macros::DjogiEnum;
    // Re-export the `#[derive(JsonbSchema)]` derive macro.
    pub use djogi_macros::JsonbSchema;
    // T11 / issue #30 — re-export the serde derives so `use djogi::prelude::*`
    // is sufficient for any `JsonbSchema`-deriving or `DjogiEnum`-deriving
    // type. The macro emits `#[derive(Serialize, Deserialize)]` paths through
    // `::djogi::__private::serde`, but adopter-side typed JSONB schemas
    // (`Jsonb<MyShape>`) derive serde directly on `MyShape`, and asking
    // adopters to add a `serde` line to their `Cargo.toml` and a separate
    // `use serde::*` clause is friction the framework can absorb.
    pub use ::serde::{Deserialize, Serialize};
    // Spatial primitive — gated behind the `spatial` feature flag.
    #[cfg(feature = "spatial")]
    pub use crate::geo::GeoPoint;
}
