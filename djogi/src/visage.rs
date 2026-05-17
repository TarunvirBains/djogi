//! Visage runtime — trait surface, error types, and sealed
//! projection metadata for generated visage structs.
//!
//! `#[model]` emits four visage structs per model
//! (`{Model}Public`, `{Model}SelfView`, `{Model}Admin`, `{Model}Export`)
//! plus conversion impls. This module holds the runtime-side types
//! those emissions depend on:
//!
//! - [`DjogiVisage`] — Phase 8.5 issue #231 — projection metadata
//!   trait. Every emitted visage carries `type Model`, `SCOPE`,
//!   `COLUMNS`, `PROJECTIONS`, and `PROJECTION_LIST` items so
//!   framework-internal code (lints, debug formatters, future
//!   Tier-2 predicate rendering) can read the projection shape —
//!   and reach the source-model table via
//!   `<V::Model as Model>::table_name()` — through a single bound.
//!   Sealed against arbitrary downstream impls via the metadata
//!   seal `private::Sealed` (re-exported as
//!   `::djogi::__private::DjogiVisageSealed` for macro emission);
//!   see the trait's own rustdoc for the convention-boundary
//!   discussion.
//! - [`projection::ProjectionEntry`] — sealed `__private` enum
//!   discriminating column entries from derived-expression entries
//!   inside `PROJECTIONS`. The `pub` visibility is mandated by the
//!   trait constant's type; the `__private` module path and
//!   `#[non_exhaustive]` attribute carry the convention seal.
//! - [`VisageError`] — the fallible-conversion error type, returned
//!   by every `TryFrom<&Model>` impl that nests a relation-form peer
//!   visage OR has a fallible derived entry. `#[non_exhaustive]`.
//! - `impl From<Infallible> for VisageError` — glue that lets
//!   generated code propagate `Infallible` with `?` when mixed-
//!   fallibility visages embed an infallible derived entry alongside
//!   a fallible one.
//!
//! No runtime execution happens here — every path is straight-line
//! error construction or formatting.

use std::convert::Infallible;

/// Visage projection-metadata trait.
///
/// Phase 8.5 issue #231 — every emitted visage struct (`UserPublic`,
/// `UserSelfView`, `UserAdmin`, `UserExport`, ...) carries a single
/// impl of this trait so framework-internal code can read the
/// projection shape — and the source-model pairing — without
/// hand-rolling parallel items per visage. Adopter code rarely
/// touches the trait directly — the items are reached on demand for
/// advanced uses (debug formatting, schema documentation
/// generation, traversal helpers that need the source table name).
///
/// # Items
///
/// - [`Model`](Self::Model) — the source model `M` the visage is a
///   projection of. The bound is `M: Model`, so generic consumers
///   reach the source table at compile time via
///   `<V::Model as crate::model::Model>::table_name()` — no parallel
///   `TABLE` constant on this trait.
/// - [`SCOPE`](Self::SCOPE) — stable scope key matching the visage's
///   audience.
/// - [`COLUMNS`](Self::COLUMNS) — names appearing at each ordinal
///   position of the visage's SELECT row, in struct-field order.
///   Column entries: the raw column name. Derived entries: the
///   entry's `name` (which equals the SELECT alias).
/// - [`PROJECTIONS`](Self::PROJECTIONS) — sealed `ProjectionEntry`
///   list. Walked by framework-internal consumers only — the
///   queryset hot path uses `PROJECTION_LIST` instead.
/// - [`PROJECTION_LIST`](Self::PROJECTION_LIST) — pre-rendered
///   comma-joined SELECT-list string. Column entries render
///   verbatim; derived entries render as `(<sql>) AS <alias>`.
///   `VisageQuerySet` splices this directly into the SELECT slot at
///   query time.
///
/// # Consignment walkthrough — Phase 8.5 #231
///
/// The motivating scenario from the spec: a `Consignment` model
/// with three storage columns (`inbound_site`, `outbound_site`,
/// `direction`) and one derived projection (`facility_site`) that
/// picks between the two sites based on `direction`. The derived
/// declaration is paired SQL + Rust — the SQL renders into the
/// SELECT projection at query time; the Rust runs in-memory on
/// `From<&Model>` construction. The parity helper catches drift
/// between the two sides.
///
/// The example below covers one pass through `From<&Model>`, one
/// pass through [`VisageQuerySet`](crate::query::VisageQuerySet)
/// (in `# fn _fetch(...)` framing — the SQL execution is not run
/// at doctest time; the integration test
/// `phase8_5_visage_derived_projection.rs` exercises the live
/// round-trip against Postgres), and one pass through the parity
/// helper.
///
/// ```no_run
/// use djogi::prelude::*;
/// use djogi::testing::{DerivedParity, assert_derived_parity_fetched};
/// use djogi::DjogiVisage;
///
/// #[model(table = "consignments_djogi_visage_doctest")]
/// #[derive(Model, Debug, Clone, PartialEq)]
/// #[derived(
///     name   = facility_site,
///     ty     = String,
///     scopes = [public, admin, export],
///     sql    = "CASE WHEN direction = 'inbound' \
///                   THEN inbound_site \
///                   ELSE outbound_site END",
///     rust   = "if model.direction == \"inbound\" { \
///                   model.inbound_site.clone() \
///               } else { \
///                   model.outbound_site.clone() \
///               }",
/// )]
/// pub struct Consignment {
///     #[field(expose(public, admin, export))]
///     pub inbound_site: String,
///     #[field(expose(public, admin, export))]
///     pub outbound_site: String,
///     #[field(expose(public, admin, export))]
///     pub direction: String,
/// }
///
/// // The trait constants are populated by the macro per visage scope.
/// // `ConsignmentPublic` includes the derived `facility_site` because
/// // its `scopes = [...]` list contains `public`.
/// assert_eq!(<ConsignmentPublic as DjogiVisage>::SCOPE, "public");
/// let columns = <ConsignmentPublic as DjogiVisage>::COLUMNS;
/// assert_eq!(columns[columns.len() - 1], "facility_site");
/// let pl = <ConsignmentPublic as DjogiVisage>::PROJECTION_LIST;
/// assert!(pl.contains("AS facility_site"));
///
/// // Generic visage consumers reach the source model — and the
/// // source table — through `V::Model`. The macro pins `type Model
/// // = Consignment` on every emitted impl, so consumers never have
/// // to thread the source model in as a separate type parameter.
/// fn source_table<V: DjogiVisage>() -> &'static str {
///     <<V as DjogiVisage>::Model as djogi::prelude::Model>::table_name()
/// }
/// assert_eq!(
///     source_table::<ConsignmentPublic>(),
///     "consignments_djogi_visage_doctest",
/// );
///
/// // The infallible `From<&Model>` builds the visage in-memory.
/// // The adopter's `rust` expression evaluates verbatim with a
/// // `let model: &Consignment = src;` rebind so `model.<field>`
/// // resolves without retouching the emitter's `src` parameter.
/// let row = Consignment {
///     inbound_site: "FAC-1".to_string(),
///     outbound_site: "WH-2".to_string(),
///     direction: "inbound".to_string(),
///     ..Default::default()
/// };
/// let in_memory: ConsignmentPublic = (&row).into();
/// assert_eq!(in_memory.facility_site, "FAC-1");
///
/// // The fetch path — `ConsignmentPublic::filter(...).fetch_one(...)`.
/// // Each visage emits its own queryset entry; the macro bakes the
/// // rendered `PROJECTION_LIST` (with derived expressions and
/// // aliases) into the queryset's SELECT slot. The fetch is async
/// // and requires a real DB; the doctest carries it in a `no_run`
/// // framing alongside the parity helper to show the full workflow
/// // — the live integration test under
/// // `tests/integration/phase8_5_visage_derived_projection.rs`
/// // executes the same shape against Postgres.
/// # async fn _fetch_workflow(
/// #     ctx: &mut DjogiContext,
/// #     in_memory: &ConsignmentPublic,
/// #     row: &Consignment,
/// # ) -> djogi::Result<()> {
/// let from_db: ConsignmentPublic = ConsignmentPublic::filter(|f| f.id().eq(row.id))
///     .fetch_one(ctx)
///     .await?;
///
/// // The sync per-visage parity helper compares ONLY derived
/// // fields — framework columns and storage columns are never
/// // compared (lossy `DateTime` round-trips would false-positive
/// // every high-precision timestamp regardless of any derived
/// // drift). Equivalently, the trait-backed dispatch under the
/// // `DerivedParity` trait is reachable from generic code.
/// in_memory.assert_derived_parity(&from_db).unwrap();
///
/// // The async convenience helper drives the fetch + delegates to
/// // the sync per-visage method in one call, lifting fetch
/// // failures into `DerivedParityError::Fetch`.
/// let target_id = row.id;
/// assert_derived_parity_fetched(in_memory, || async {
///     ConsignmentPublic::filter(|f| f.id().eq(target_id))
///         .fetch_one(ctx)
///         .await
/// })
/// .await
/// .unwrap();
/// # Ok(())
/// # }
/// ```
///
/// See also: the integration test
/// `tests/integration/phase8_5_visage_derived_projection.rs` for
/// the end-to-end live round-trip, and
/// [`crate::testing::assert_derived_parity_fetched`] for the
/// async convenience helper.
///
/// # Note on the absence of `TABLE`
///
/// There is intentionally no `TABLE` constant on this trait. Generic
/// callers reach the source-model table through
/// `<V::Model as crate::model::Model>::table_name()`, which is the
/// canonical entry point that already factors in compile-time table
/// validation, identifier checks, and the rest of the `Model`
/// contract. Adding a parallel `TABLE` const would duplicate state
/// the supertrait already pins.
///
/// # Relation to [`DjogiVisageOf<M>`](crate::visage_boundary::DjogiVisageOf)
///
/// `DjogiVisageOf<M>` is a marker trait sealing the visage ↔ model
/// pairing without an associated `Model` slot — bound positionally
/// on `M`. `DjogiVisage` carries the same pairing through its
/// `type Model` associated type instead, so generic code can pick
/// the spelling that fits the call site:
///
/// - `fn foo<V: DjogiVisageOf<M>, M: Model>(...)` — explicit `M`
///   parameter, ergonomic when the caller already has an `M` in
///   scope.
/// - `fn foo<V: DjogiVisage>(...)` — `V::Model` is reachable
///   internally, ergonomic when the caller only ever names the
///   visage.
///
/// The reflexive blanket
/// `impl<M: Model> DjogiVisageOf<M> for M` lets the marker accept
/// the model itself as a "degenerate visage". That blanket is the
/// reason `DjogiVisageOf<Self::Model>` alone is **not** a closed-
/// world seal on `DjogiVisage` — any `M: Model` already satisfies
/// `DjogiVisageOf<M>` reflexively, so a hand-rolled
/// `impl DjogiVisage for MyModel { type Model = Self; ... }` would
/// otherwise pass the pairing supertrait. The closed-world gate for
/// `DjogiVisage` therefore lives on a separate metadata-only seal
/// described in the next section; the `DjogiVisageOf<Self::Model>`
/// supertrait stays because it carries the visage ↔ source-model
/// pairing useful for generic code that only names `V`.
///
/// # Sealed via `private::Sealed` — metadata-only seal
///
/// `DjogiVisage` carries `private::Sealed` (module-private to
/// [`crate::visage`]) as its second supertrait. Unlike the
/// `visage_boundary::private::Sealed<M>` companion seal — which has
/// a reflexive `impl<M: Model> Sealed<M> for M` blanket so models
/// satisfy `DjogiVisageOf<Self>` "for free" — the metadata seal has
/// **no reflexive blanket**. The single emitter is the `#[model]`
/// proc macro, which routes through
/// `::djogi::__private::DjogiVisageSealed` (the convention-boundary
/// re-export) per the macro-path-routing convention.
///
/// **Seal-by-convention caveat.** Hand-implementing
/// `djogi::__private::DjogiVisageSealed` from downstream code is
/// outside the public contract; the framework reserves the right
/// to break that code in any future release without notice. This
/// matches the convention already established by `VisageSealed`,
/// `__private::pk_seal`, and `hooks::__seal::Sealed` — Rust has no
/// way to mark a trait "implementable only inside this crate"
/// when its supertrait must be reachable from a separate proc-
/// macro-emitting crate, and we accept the convention-level seal
/// as the trade-off. The `__private` module's documented contract
/// ("downstream code reaching in is breaking the framework
/// boundary") is the line of conduct that the seal rests on.
///
/// [`DjogiVisageOf<Self::Model>`]: crate::visage_boundary::DjogiVisageOf
pub trait DjogiVisage:
    crate::visage_boundary::DjogiVisageOf<<Self as DjogiVisage>::Model> + private::Sealed
{
    /// Source model the visage is a projection of. Every macro-emitted
    /// `impl DjogiVisage for {Visage}` sets `type Model = {Source}`
    /// where `{Source}` is the host `#[model]` struct. Generic code
    /// reaches the source table via
    /// `<V::Model as crate::model::Model>::table_name()`, which is the
    /// motivating reason the associated type exists at all — without
    /// it, generic visage consumers would have to thread the source
    /// model in as a separate type parameter at every call site.
    ///
    /// # Visibility note
    ///
    /// When a model is declared less-public than its generated
    /// visage (e.g. a `pub(crate) struct Inner` paired with `pub
    /// struct InnerPublic`), rustc's `private_interfaces` lint
    /// fires on the macro-emitted impl. Mirror the visage
    /// visibility on the model — `pub struct Inner` — or
    /// `#[allow(private_interfaces)]` on the source if the model
    /// must stay private. Rust does not provide a way to hide an
    /// associated type's binding while leaving the trait public,
    /// and the `<V::Model as Model>::table_name()` access pattern
    /// that the original spec requires depends on the binding
    /// being nameable.
    type Model: crate::model::Model;

    /// Stable scope key (`"public"` / `"self_view"` / `"admin"` /
    /// `"export"`). A `&'static str` rather than a typed enum to
    /// match the existing `SCOPES` tuple shape inside
    /// `djogi-macros::model::visages::SCOPES` and to avoid
    /// introducing a sibling enum to `VisageError`'s string-typed
    /// `scope` field. A future phase may swap this to a sealed
    /// `enum djogi::Scope` once the surrounding surface justifies
    /// the migration.
    const SCOPE: &'static str;

    /// Names that appear at each ordinal position of the visage's
    /// SELECT row, in struct-field order. For column entries this
    /// is the raw column name; for derived entries this is the
    /// entry's `name` (which equals the SELECT alias emitted into
    /// the projection).
    ///
    /// This **is** the visage's `FromPgRow::COLUMNS` — the visage's
    /// `FromPgRow` impl re-exports the same slice so the positional
    /// decoder's debug-build name guard compares against the same
    /// alias the SELECT emitted.
    ///
    /// The historical
    /// `FromPgRow::COLUMN_LIST == COLUMNS.join(", ")` invariant
    /// becomes `FromPgRow::COLUMN_LIST == PROJECTION_LIST` for
    /// visages — the only callers that interpolated `COLUMN_LIST`
    /// directly were the visage queryset builders, which now route
    /// through `PROJECTION_LIST` instead.
    const COLUMNS: &'static [&'static str];

    /// Full projection (columns and derived expressions) in
    /// struct-field order. **Metadata-only** — walked by
    /// framework-internal consumers (lints, debug formatters, future
    /// Tier-2 per-entry SQL renderer). The queryset hot path uses
    /// [`PROJECTION_LIST`](Self::PROJECTION_LIST) instead.
    ///
    /// Adopters do not name `ProjectionEntry` directly — the type
    /// is `pub` to satisfy the trait constant's type, but lives
    /// under `__private` and carries a "do-not-construct"
    /// convention warning matching `__private::VisageSealed` and
    /// `__private::pk_seal`.
    const PROJECTIONS: &'static [crate::__private::ProjectionEntry];

    /// Rendered SQL projection list rendered once at macro time
    /// (e.g. `"id, name, (CASE ... END) AS facility_site"`).
    /// `VisageQuerySet` splices this single string into the SELECT
    /// slot at query time — no runtime walk over `PROJECTIONS`.
    /// Equal to `COLUMNS.join(", ")` when there are no derived
    /// entries (because the column entry's name and its alias
    /// coincide).
    const PROJECTION_LIST: &'static str;
}

/// Closed-world metadata seal for [`DjogiVisage`].
///
/// Crate-private — adopter code cannot name
/// `crate::visage::private::Sealed` directly. The single externally
/// reachable path is `::djogi::__private::DjogiVisageSealed`
/// (re-exported through the `#[doc(hidden)] __private` convention
/// boundary), and macro-emitted code routes through that path per
/// `feedback_macro_path_routing.md`. `pub(crate)` is the
/// visibility floor `lib.rs` needs to `pub use`-route the trait
/// through `__private` while keeping the `crate::visage::private`
/// path itself non-public — `pub mod private` would expose a
/// second public path; `mod private` would block the
/// `pub use crate::visage::private::Sealed` re-export at the
/// crate root.
///
/// Distinct from [`crate::visage_boundary::private::Sealed<M>`] (the
/// pairing seal underneath `DjogiVisageOf<M>`): the pairing seal
/// carries a reflexive `impl<M: Model> Sealed<M> for M` blanket so
/// models satisfy "a visage of themselves" trivially. **This seal
/// has no reflexive blanket** — only `#[model]`-emitted
/// `impl DjogiVisageSealed for {Visage}` blocks satisfy it, which
/// closes the convention-level gate against downstream code writing
/// `impl DjogiVisage for MyModel { type Model = Self; ... }` to
/// fabricate a visage that never went through the macro's emission
/// path.
pub(crate) mod private {
    /// Closed-world metadata seal — only `#[model]`-emitted code
    /// (routed through `::djogi::__private::DjogiVisageSealed`)
    /// satisfies this trait. The module is `pub(crate)` so the
    /// `__private` re-export can resolve from the crate root, but
    /// downstream code cannot name `djogi::visage::private` from
    /// outside the crate — the single externally reachable path
    /// remains `::djogi::__private::DjogiVisageSealed`.
    pub trait Sealed {}
}

/// Sealed projection metadata enum.
///
/// `ProjectionEntry` is `pub` (the [`DjogiVisage::PROJECTIONS`]
/// trait constant requires the enum to be nameable through
/// `::djogi::__private::ProjectionEntry`) but lives behind
/// `crate::__private` and carries a **convention-only seal** — the
/// same precedent as the existing `__private::VisageSealed` and
/// `__private::pk_seal` surfaces. The `#[non_exhaustive]`
/// attribute prevents exhaustive `match` construction across the
/// crate boundary even when adopters do reach in. The `__private`
/// module hiding plus the `#[doc(hidden)]` on variants removes the
/// type from the rustdoc surface.
///
/// The "do not construct or match on this type" warning is the
/// seal at the language-of-conduct level — it does not mechanically
/// prevent construction, but it matches the precedent the framework
/// already establishes for its other internal-boundary types.
pub mod projection {
    /// Sealed projection-entry discriminant — **do not construct or
    /// match on this type from downstream code.**
    ///
    /// The variants are public only because the
    /// [`DjogiVisage::PROJECTIONS`](crate::DjogiVisage::PROJECTIONS)
    /// trait constant requires the enum to be nameable through
    /// `::djogi::__private::ProjectionEntry`; reaching this type
    /// from outside the macro-emitted path is breaking the framework
    /// boundary, and the framework reserves the right to change the
    /// variants in any future release without notice. Same
    /// convention as `__private::VisageSealed` and
    /// `__private::pk_seal` — the warning is the seal.
    #[non_exhaustive]
    pub enum ProjectionEntry {
        /// A direct column reference. The string is the column
        /// name as it appears in the projection.
        #[doc(hidden)]
        Column(&'static str),
        /// A derived projection entry — Phase 8.5 issue #231. The
        /// `alias` is the SELECT alias the macro emitted (which
        /// equals the visage struct's field name and the entry's
        /// `COLUMNS[i]` slot); `sql` is the adopter's SQL expression
        /// verbatim, before the macro wraps it in outer parens for
        /// the SELECT splice.
        ///
        /// This shape carries `alias` + `sql` only because the
        /// framework-internal consumers (lints, debug formatters,
        /// future Tier-2 per-entry SQL renderer) need those two
        /// fields. Documentation generators and the `djogi docs`
        /// CLI will consume a richer descriptor / inventory surface
        /// in a later stage; this metadata enum is intentionally not
        /// that public descriptor surface.
        #[doc(hidden)]
        Derived {
            /// The SELECT alias (= visage struct field name).
            alias: &'static str,
            /// The adopter's SQL expression verbatim. The outer
            /// parens the SELECT renderer adds happen at projection-
            /// list assembly time, not here.
            sql: &'static str,
        },
    }
}

/// Error returned by a fallible visage conversion (`TryFrom<&Model>`).
///
/// Generated by codegen whenever a visage nests at least one peer
/// visage via a relation field OR carries at least one fallible
/// derived entry (Phase 8.5 issue #231). `#[non_exhaustive]` is
/// intentional: protected-data governance and later phases may add
/// variants (e.g. redaction failures, codec errors) without
/// breaking the public API.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum VisageError {
    /// A relation field was projected before the relation was loaded.
    ///
    /// Fix: call `.prefetch(|r| r.<field>())` (or `.select_related(...)` for
    /// SQL-JOIN eager loading) on the queryset before invoking
    /// `TryFrom::try_from(&model)`.
    #[error(
        "visage of {model}.{field} for scope `{scope}` failed: \
         relation is not resolved (call .prefetch() before projecting)"
    )]
    UnresolvedRelation {
        /// Name of the owning model (`struct` ident).
        model: &'static str,
        /// Name of the relation field on the owning model.
        field: &'static str,
        /// Visage scope whose `TryFrom` impl raised this error
        /// (`public` / `self_view` / `admin` / `export`).
        scope: &'static str,
    },

    /// A derived field declared as NOT NULL (`ty = T`) decoded NULL
    /// from the database row.
    ///
    /// Surfaces from the visage's `FromPgRow` impl when Postgres
    /// returns a NULL value for the position of a derived entry
    /// whose Rust type is not `Option<_>`. Wrapped via the existing
    /// `impl From<VisageError> for DjogiError` blanket — callers
    /// fetching through `VisageQuerySet` see this as
    /// `DjogiError::Visage(VisageError::DbComputedNullForNonOptional { .. })`.
    ///
    /// Fix: either declare `ty = Option<T>` on the `#[derived(...)]`
    /// attribute (the spec's null-tolerant shape) or fix the SQL
    /// expression to coalesce the NULL on the server side
    /// (`COALESCE(<expr>, <default>)`).
    #[error(
        "visage `{visage}` derived field `{field}` returned NULL but is declared \
         as a non-optional type (use `ty = Option<...>` to permit NULL)"
    )]
    DbComputedNullForNonOptional {
        /// Visage type name (e.g. `"ConsignmentPublic"`).
        visage: &'static str,
        /// The derived field name that decoded NULL
        /// (e.g. `"facility_site"`).
        field: &'static str,
    },

    /// A derived field's runtime type did not match the declared
    /// `ty`.
    ///
    /// Surfaces from the visage's `FromPgRow` impl when Postgres
    /// returns a value the declared Rust type cannot accept. The
    /// `expected` carries the declared type's name; `actual` carries
    /// the Postgres-side type description as best as the row-
    /// decoder can recover.
    ///
    /// Fix: align the `ty = ...` on the `#[derived(...)]` attribute
    /// with the SQL expression's result type, or cast the SQL
    /// expression on the server side to the expected type.
    #[error(
        "visage `{visage}` derived field `{field}` type mismatch: \
         expected {expected}, got {actual}"
    )]
    DbComputedTypeMismatch {
        /// Visage type name (e.g. `"ConsignmentPublic"`).
        visage: &'static str,
        /// The derived field name that failed to decode
        /// (e.g. `"facility_site"`).
        field: &'static str,
        /// Declared Rust type name (e.g. `"Site"`).
        expected: &'static str,
        /// Postgres-side type description (e.g. `"INTEGER"`).
        actual: &'static str,
    },
}

/// Unreachable — `Infallible` has no inhabitants. The impl exists so
/// generated `TryFrom` chains with `?` can coerce the blanket-provided
/// `Infallible` error into `VisageError` uniformly.
impl From<Infallible> for VisageError {
    fn from(never: Infallible) -> Self {
        match never {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unresolved_relation_display_includes_all_context() {
        let err = VisageError::UnresolvedRelation {
            model: "Vehicle",
            field: "owner",
            scope: "public",
        };
        let msg = err.to_string();
        assert!(msg.contains("Vehicle"));
        assert!(msg.contains("owner"));
        assert!(msg.contains("public"));
        assert!(msg.contains(".prefetch()"));
    }

    #[test]
    fn infallible_converts_to_visage_error() {
        fn accepts_from<T: From<::std::convert::Infallible>>() {}
        accepts_from::<VisageError>();
    }

    #[test]
    fn db_computed_null_for_non_optional_display_includes_context() {
        let err = VisageError::DbComputedNullForNonOptional {
            visage: "ConsignmentPublic",
            field: "facility_site",
        };
        let msg = err.to_string();
        assert!(msg.contains("ConsignmentPublic"));
        assert!(msg.contains("facility_site"));
        assert!(msg.contains("Option"));
    }

    #[test]
    fn db_computed_type_mismatch_display_includes_context() {
        let err = VisageError::DbComputedTypeMismatch {
            visage: "ConsignmentPublic",
            field: "facility_site",
            expected: "Site",
            actual: "INTEGER",
        };
        let msg = err.to_string();
        assert!(msg.contains("ConsignmentPublic"));
        assert!(msg.contains("facility_site"));
        assert!(msg.contains("Site"));
        assert!(msg.contains("INTEGER"));
    }
}
