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

// Alias the current crate as `djogi` under test so the absolute
// `::djogi::*` paths emitted by `#[djogi_test]` (and any other macro that
// hard-codes the crate name) resolve when used from inside this crate's
// own unit-test modules. Outside of tests, the dependency graph already
// carries the name; under `cargo test --lib` the crate root has no
// `djogi` entry without this self-extern.
#[cfg(test)]
extern crate self as djogi;

#[doc(hidden)]
pub mod __bypass;
pub mod apps;
pub mod array;
pub mod auth;
pub mod cache;
pub mod compose;
pub mod config;
pub mod context;
pub mod descriptor;
pub mod enum_;
pub mod error;
pub mod expr;
pub mod field_codec;
pub mod fts;
pub mod fts_query;
#[cfg(feature = "spatial")]
pub mod geo;
pub mod hooks;
pub(crate) mod ident;
pub mod intent;
pub mod jsonb;
pub mod live_migrate;
pub mod migrate;
pub mod model;
#[cfg(feature = "notify")]
pub mod notify;
pub mod outbox;
pub mod pg;
pub mod primary_key;
pub mod query;
pub mod relation;
pub mod snapshot;
pub mod testing;
pub mod tracked;
pub mod trait_registry;
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

// Cluster 8δ T7.4 — `SassiBootHook` re-export so `#[derive(Model)]`-emitted
// `inventory::submit!` blocks can spell `::djogi::SassiBootHook` per
// `feedback_macro_path_routing.md` (macro paths route through djogi only).
pub use crate::cache::SassiBootHook;

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
    pub use tokio;
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
        pub use crate::pg::decode::{FromJoinedPgRow, FromPgRow, decode_at, try_get_scalar};
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

    /// Hook-dispatch re-exports for the `#[model(hooks)]` macro (T1.3).
    ///
    /// The macro-emitted code routes through `::djogi::__private::hooks::*`
    /// rather than `::djogi::hooks::*` so the seal supertrait
    /// (`Sealed`, otherwise unnameable from outside the `djogi` crate)
    /// is reachable in the macro's emission context. Adopter code uses
    /// the public surface — `djogi::ModelHooks` for the trait one
    /// implements, `djogi::hooks::HasHooks` for trait bounds — and never
    /// touches this module.
    ///
    /// Per `feedback_macro_path_routing.md`: macro-emitted paths route
    /// through `::djogi::*` only; the macro never reaches into
    /// `::heeranjid::*` / `::time::*` / `::uuid::*` / etc. directly.
    pub mod hooks {
        pub use crate::hooks::__seal::{MarkerSeal, Sealed};
        pub use crate::hooks::HasHooks;
        pub use crate::hooks::ModelHooks;
    }

    /// `tracing` re-export for macro-generated `_insecurely()` warn! calls.
    ///
    /// Routing through `::djogi::__private::tracing` keeps user crates from
    /// needing `tracing` as a direct dependency — the same path-routing
    /// convention used for `inventory`, `postgres_types`, and `futures`.
    pub use tracing;

    /// Q-algebra seal extension — routed through `__private` so the
    /// proc-macro's emitted `IntoQ<#model>` impl for `{Model}Filter`
    /// can satisfy the crate-private `sealed_into_q::Sealed`
    /// supertrait from inside the adopter crate.
    ///
    /// Adopter code never names this trait — `IntoQ<T>` is the public
    /// surface. Only `djogi-macros::model::filter` reaches in here,
    /// and only to stamp the seal on the `{Model}Filter` types it
    /// generates.
    ///
    /// See `crate::query::q::__SealedIntoQ` for the underlying
    /// crate-private seal, and `query::q::IntoQ` for the public
    /// trait downstream code consumes.
    pub use crate::query::q::__SealedIntoQ;

    /// Re-exports needed by the `IntoQ<#model>` impl macro emission
    /// in `djogi-macros::model::filter`. Adopter code uses
    /// `crate::query::filter::clauses_into_condition` ↔ `Q::Condition(_)`
    /// indirectly via `filter_struct(my_filter)`; the helper exists at
    /// this path so macro-emitted code can route through
    /// `::djogi::__private::query::*` per `feedback_macro_path_routing.md`.
    ///
    /// Phase 8eta PR2a additions (consumed by PR2b's direct-`Q<T>` SQL
    /// walker and PR2d's macro override):
    ///
    /// - `SqlEmitContext` — the parent-table-threading context PR2d's
    ///   generated `__djogi_emit_field_predicate` arms expect.
    /// - `PortablePredicateError` — the typed lowering error PR2b's
    ///   walker propagates back to `DjogiError`.
    /// - `__make_djogi_field` — the macro constructor PR3 will route every
    ///   generated `{Model}Fields` accessor through.
    pub mod query {
        pub use crate::query::field::djogi_field_macro_support::__make_djogi_field;
        pub use crate::query::filter::clauses_into_condition;
        pub use crate::query::portable::{PortablePredicateError, SqlEmitContext};
        pub use crate::query::q::{IntoQ, Q};

        // Phase 8eta PR2b — hidden re-export of the portable SQL helper
        // module. PR2d's macro-emitted
        // `Model::__djogi_emit_field_predicate` override consumes
        // `::djogi::__private::query::portable_emit::*`. The helpers
        // themselves live at `crate::query::portable::emit::*` (a
        // hidden public submodule); the re-export here keeps macro
        // path-routing through `::djogi::*` per
        // `feedback_macro_path_routing.md`.
        pub use crate::query::portable::emit as portable_emit;
    }
}

pub use apps::{App, AppDescriptor, AppIdentity, AppRegistry, CrossAppEdge};
// `AppDiagnostic` is reserved for future migration consumers (compose / status
// / attune) per its module doc — currently not wired in production. Keep
// the symbol available cross-crate but hide it from rustdoc until a real
// consumer lands and the variant set stabilises.
#[doc(hidden)]
pub use apps::AppDiagnostic;
// Phase 8 §T2.1 — composition primitives. The runtime trait surfaces.
// `Auditable` impls are emitted by `#[model(auditable)]` (T2.4 — the
// surface superseded T2.2's `#[derive(Auditable)]` per spec line 1037,
// locked 2026-05-03); `SoftDeletable` impls are emitted by
// `#[model(soft_deletable)]` (T2.6 — the surface superseded T2.3's
// `#[derive(SoftDeletable)]` for the same proc-macros-cannot-observe-
// sibling-derives constraint).
pub use compose::{Auditable, SoftDeletable};
pub use context::DjogiContext;
pub use descriptor::{
    ComputedFieldDescriptor, DefaultVolatility, DeferrabilitySpec, EnumDescriptor, FieldDescriptor,
    FieldSqlType, GeographySubtype, IndexColumnSpec, IndexKind, IndexNameKind, IndexNameTarget,
    IndexNullsOrder, IndexOrder, IndexSpec, IndexTarget, IndexType, ModelDescriptor, PartitionSpec,
    PkType, ProtectedFieldMetadata, RedactionPolicy, RetentionLabel, Sensitivity, index_name,
};
// Top-level `djogi::GeoPoint` re-export for spatial models. Feature-gated so
// the symbol does not appear in default-feature builds or `cargo doc` output
// when PostGIS support is not requested.
pub use djogi_macros::{
    DjogiEnum, JsonbSchema, apps, deliberately_bypass_convention_with_raw_sql, many_to_many,
    primary_key, reverse_one_to_many, reverse_one_to_one, trait_impl,
};
#[cfg(feature = "spatial")]
pub use geo::GeoPoint;
pub use hooks::ModelHooks;
pub use jsonb::{Jsonb, JsonbPathRef, JsonbSchema, UnknownField, UnknownFieldExt};
// `FromPgRow` is the canonical row-decode trait — adopters write
// `ctx.raw_query::<MyType>(...)` against it, so it stays in the public
// rustdoc surface. The other four below are macro-emission targets and
// raw helpers that adopter code does not implement directly: macros emit
// `impl ::djogi::pg::decode::FromJoinedPgRow for T`; `try_get_scalar` is
// used by the `djogi::primary_key!` macro for newtype-PK decode.
// `FromRowTuple` + `try_get_tuple` are module-internal today (the public
// `raw_query` bound is `FromPgRow`, not `FromRowTuple`). Hide them from
// rustdoc; keep the symbols available for macro emission and any future
// raw-tuple-decode promotion.
pub use pg::decode::FromPgRow;
#[doc(hidden)]
pub use pg::decode::{FromJoinedPgRow, try_get_scalar};
pub use primary_key::{PrimaryKey, PrimaryKeyClientGen, PrimaryKeyDbGen};
// The `#[djogi_test]` attribute macro re-exported for convenience. The macro
// itself is always available (proc macros have no runtime component); the
// *runtime helper* it calls (`::djogi::testing::setup_test_db`) is gated on
// `cfg(any(test, feature = "testing"))` so the generated code only compiles
// in test or feature-enabled builds.
pub use djogi_macros::djogi_test;
pub use error::{DbError, DjogiError};
pub use expr::{
    AggregateExpr, Case, CaseBuilder, DenseRank, Exists, Expr, OuterRef, QualifyCondition,
    QualifyOp, Rank, RowNumber, Subquery, WindowRanking,
};
// Field-level codec public surface. `FieldCodec` is the trait adopters
// implement for at-rest column transformations.
pub use field_codec::FieldCodec;
// `is_codec_registered` is consumed by the macro layer to validate
// `#[field(codec = "…")]` strings at expansion time. Adopter code does
// not call it directly. Hide from rustdoc; keep the symbol available
// cross-crate for macro emission.
#[doc(hidden)]
pub use field_codec::is_registered as is_codec_registered;
pub use fts::{FtsDescriptor, TsQuery, TsVector};
pub use fts_query::FtsFieldRef;
// Cluster 8γ Stage 2 (T6.9b): `Condition` retired from the crate
// root re-export. The public predicate substrate is `Q<T>`; the
// legacy `Condition` type lives at `djogi::query::internal::Condition`
// for the few cross-cluster consumers that still name it (8β's
// `default_filter_condition` trait method, integration tests
// asserting on tree shape). Adopter code composes through `Q<T>` and
// never reaches for `Condition` directly.
pub use query::{
    AggregateQuery, AnnotatedQuerySet, ArrayPredicate, BasicPredicate, CachedPortableQuerySet,
    ClosureModel, FieldRef, FilterClause, IntoAggregateTuple, IntoFilterValue, Lookup,
    MaterializeClosureOptions, MaterializeClosureReport, ModelCursorStream, ModelFilter, OrderExpr,
    PortableQuerySet, Q, QuerySet, RawCursorStream, RecursiveDirection, RecursiveQuerySet,
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
    #[doc(hidden)]
    pub use crate::apps::AppDiagnostic;
    pub use crate::apps::{App, AppDescriptor, AppIdentity, AppRegistry, CrossAppEdge};
    // Phase 8 §T2.1 — composition primitives (see crate root re-export).
    pub use crate::compose::{Auditable, SoftDeletable};
    pub use crate::context::DjogiContext;
    pub use crate::descriptor::{
        DefaultVolatility, DeferrabilitySpec, EnumDescriptor, FieldDescriptor, FieldSqlType,
        GeographySubtype, IndexColumnSpec, IndexKind, IndexNullsOrder, IndexOrder, IndexSpec,
        IndexTarget, IndexType, ModelDescriptor, PartitionSpec, PkType, ProtectedFieldMetadata,
        RedactionPolicy, RetentionLabel, Sensitivity,
    };
    pub use crate::error::{DbError, DjogiError};
    pub use crate::expr::{
        AggregateExpr, Case, CaseBuilder, DenseRank, Exists, Expr, OuterRef, QualifyCondition,
        QualifyOp, Rank, RowNumber, Subquery, WindowRanking,
    };
    // `FieldCodec` is the trait adopters implement when declaring a
    // codec — belongs in the prelude because protected-field
    // declarations live in adopter model files. `is_codec_registered`
    // is the lookup the macro layer uses to validate
    // `#[field(protected(codec = "<id>"))]` at expansion time;
    // adopter code does not call it directly.
    pub use crate::field_codec::FieldCodec;
    #[doc(hidden)]
    pub use crate::field_codec::is_registered as is_codec_registered;
    pub use crate::fts::{FtsDescriptor, TsQuery, TsVector};
    pub use crate::fts_query::FtsFieldRef;
    pub use crate::hooks::ModelHooks;
    pub use crate::jsonb::{Jsonb, JsonbPathRef, JsonbSchema, UnknownField, UnknownFieldExt};
    pub use crate::model::Model;
    pub use crate::pg::decode::FromPgRow;
    #[doc(hidden)]
    pub use crate::pg::decode::{FromJoinedPgRow, try_get_scalar};
    // Cluster 8γ Stage 2 (T6.9b): `Condition` retired from the
    // prelude. Adopter code composes through `Q<T>` (in this list);
    // legacy `Condition` callers reach `djogi::query::internal::Condition`.
    pub use crate::query::{
        AggregateQuery, AnnotatedQuerySet, ArrayPredicate, BasicPredicate, CachedPortableQuerySet,
        ClosureModel, FieldRef, FilterClause, IntoAggregateTuple, IntoFilterValue, Lookup,
        MaterializeClosureOptions, MaterializeClosureReport, ModelFilter, OrderExpr,
        PortableQuerySet, Q, QuerySet, RecursiveDirection, RecursiveQuerySet, VisageQuerySet,
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
    // Phase 8 §T2.6 — `#[derive(SoftDeletable)]` was retired in favour
    // of `#[model(soft_deletable)]` (mirrors the T2.4 Auditable pivot).
    // The runtime trait `SoftDeletable` re-export above (via
    // `crate::compose::*`) stays — only the derive surface goes away.
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
