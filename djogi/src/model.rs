//! The `Model` trait — the contract every `#[model]` struct satisfies.
//!
//! All CRUD methods take `&mut DjogiContext`, so the same call site works against a
//! pool or inside a transaction without re-borrows or type juggling:
//!
//! ```ignore
//! // Pool-backed:
//! let mut ctx = DjogiContext::from_pool(pool.clone());
//! let post = Post::create(&mut ctx, post).await?;
//!
//! // Transaction-backed via the `atomic` free function (re-exported through
//! // `djogi::prelude`). The closure must return `Pin<Box<dyn Future<…>>>`:
//! atomic(&mut ctx, |tx| Box::pin(async move {
//!     Post::create(tx, post).await?;
//!     Ok(())
//! })).await?;
//! ```
//!
//! ## Context dispatch
//!
//! Each CRUD method takes `&mut DjogiContext` by mutable reference. The body
//! pattern-matches on [`DjogiContext::inner_mut`](crate::DjogiContext) to dispatch
//! to the right query path — pool-backed contexts check out a connection,
//! transaction-backed ones reuse the open transaction. See `djogi::context`
//! module docs for the full rationale.
//!
//! ## Send bounds
//!
//! The returned `Future` types carry `+ Send` explicitly so callers can `.await`
//! them across task boundaries. `&mut DjogiContext` is itself `Send` because
//! `DjogiContext` only holds `Send` data (a `DjogiPool`, which is `Arc`-backed
//! and `Send`, or an open `tokio_postgres` transaction client, which is `Send`).
//!
//! ## Single-value `Pk`
//!
//! The associated `Pk` type is a single SQL-bindable value (`Encode + Type`).
//! Composite primary keys (`#[model(pk = ["field_a", "field_b"])]`) are
//! declared in `PkType::Composite` for the descriptor but are deferred to
//! Phase 2, where they require a different `get()` signature backed by the
//! QuerySet filter API. Phase 1 emits a "not yet supported" compile error
//! if a user sets a composite PK.

use crate::DjogiError;
use crate::context::DjogiContext;
use crate::descriptor::ModelDescriptor;
use std::future::Future;

/// Seal marker for the [`Model`] trait.
///
/// The `#[model(...)]` attribute macro emits
/// `impl djogi::model::__sealed::Sealed for T {}` alongside the
/// `impl Model for T` block. A hand-rolled `impl Model` that skips
/// `#[model(...)]` fails to compile because the sealed supertrait is
/// unsatisfied — so hostile downstream code cannot fabricate a `Model`
/// whose `table_name()` or `descriptor().fields[].name` smuggles SQL
/// into the emitter's `SqlAccumulator::push_sql` sites. The sibling
/// `#[derive(Model)]` proc-macro is a no-op stub kept as a placeholder
/// for future derive-based extensions; it does not produce a working
/// `Model` impl. The module is `#[doc(hidden)] pub` because `djogi-macros`
/// emits a cross-crate path through it; the `__` prefix plus the
/// seal-marker doc comment are the social signal that downstream code
/// must never reach into it directly. The threat model defends against
/// accidental hand-impls, not deliberate framework subversion (which
/// has simpler routes via `unsafe`).
#[doc(hidden)]
pub mod __sealed {
    pub trait Sealed {}
}

/// The contract every adopter struct that participates in djogi's data layer
/// satisfies. Implemented exclusively by the `#[model(...)]` attribute macro
/// re-exported through `djogi::prelude` (the sibling `#[derive(Model)]`
/// proc-macro is a no-op stub); the sealed `__sealed::Sealed` supertrait
/// makes hand-rolled `impl Model` blocks unsatisfiable, so every `Model` in
/// production code carries the full derivation chain (descriptor emission,
/// `FromPgRow`, the `{Model}Fields` / `{Model}Filter` companions, registration).
///
/// # What implementing `Model` gives the adopter
///
/// - **Single-row CRUD.** [`Model::create`], [`Model::get`], [`Model::save`],
///   [`Model::delete`], [`Model::refresh_from_db`] — every method takes
///   `&mut DjogiContext` so the same call site works against a pool-backed
///   context or a transaction-backed one (the framework pattern-matches on
///   the inner variant at each `tokio_postgres` boundary).
/// - **The queryset entry point.** [`Model::objects`] returns a lazy
///   [`QuerySet<Self>`](crate::query::QuerySet) — filters, ordering,
///   pagination, distinct, bulk update, bulk delete. Nothing hits the database
///   until a terminal method is called.
/// - **Descriptor emission.** The macro emits a `ModelDescriptor` via
///   `inventory::submit!`, registering the struct with the workspace's
///   migration differ, app registry, admin console, and shell bindings — all
///   without any explicit registration call by the adopter.
/// - **Row decode.** A canonical `impl FromPgRow for Self` is emitted so any
///   raw-SQL escape hatch (under the bypass attribute — see
///   [`docs/spec/raw-sql-escape-hatches.md`](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md))
///   can decode rows into the model with positional, debug-asserted column
///   reads.
///
/// # How to implement (and the only way to)
///
/// Adopters never write `impl Model for MyType` by hand. The sealed
/// supertrait blocks it at compile time. Use the `#[model(...)]` attribute
/// macro re-exported through `djogi::prelude`:
///
/// ```ignore
/// use djogi::prelude::*;
///
/// #[model(table = "articles")]
/// pub struct Article {
///     pub title: String,
///     pub body: String,
///     pub published: bool,
/// }
/// ```
///
/// The macro injects the selected primary key type (`HeerIdRecencyBiased` /
/// `HeerIdDesc` by default), `created_at: DateTime`, and
/// `updated_at: DateTime` as real public struct fields, generates the `Model`
/// impl, the `FromPgRow` impl, the `ArticleFields` / `ArticleFilter` /
/// `ArticleRelated` companion types, and submits the descriptor via
/// `inventory::submit!` for app/migration registration.
///
/// # Where to read further
///
/// - **Specification** — [`docs/spec/models.md`](https://github.com/tarunvir/djogi/blob/main/docs/spec/models.md)
///   for the formal `Model` contract, framework field semantics, and the
///   `pk = ...` configuration matrix.
/// - **Getting started** — [`docs/guide/getting-started.md`](https://github.com/tarunvir/djogi/blob/main/docs/guide/getting-started.md)
///   for an end-to-end walkthrough.
/// - **Crate root rustdoc** — module table summarising the public surface.
///
/// # Why the seal
///
/// Every `Model` method composes through emitter sites that trust
/// `Self::table_name()` and `Self::descriptor().fields[].name` to be
/// well-formed identifiers. A hand-rolled `impl Model` could smuggle hostile
/// strings into those positions; the seal removes that route entirely.
/// Threat model: defends against accidental hand-impls, not deliberate
/// framework subversion (which has simpler routes via `unsafe`).
pub trait Model: Sized + Send + Sync + 'static + __sealed::Sealed {
    /// Primary key Rust type.
    /// - `pk = HeerIdRecencyBiased` (default, Phase 7-Zero-2 T2) → `HeerIdDesc`
    /// - `pk = HeerId` → `HeerId` (ascending 64-bit)
    /// - `pk = RanjIdRecencyBiased` → `RanjIdDesc`
    /// - `pk = RanjId` → `RanjId` (heeranjid's UUIDv8 newtype)
    /// - `pk = Serial` → `i32`
    /// - `pk = None` → NO `impl Model`. `()` cannot satisfy the
    ///   `postgres_types::ToSql` bound below, and a dummy newtype
    ///   would misrepresent the model's actual key shape. `pk = None` models
    ///   still get struct injection, `FromRow`, and descriptor registration
    ///   — they just don't get CRUD methods. A future phase will introduce
    ///   a separate trait for composite/user-managed PKs.
    type Pk: Clone
        + Send
        + Sync
        + postgres_types::ToSql
        + for<'a> postgres_types::FromSql<'a>
        + 'static;

    /// Compile-time field handle bag. Generated by `#[model]` as
    /// `{Model}Fields` — a ZST whose root-column methods return
    /// [`crate::query::DjogiField<Self, V>`] after Phase 8eta PR3.
    /// SQL-only path-aware traversal lives on the generated
    /// `{Model}SqlFields` sibling.
    ///
    /// `Default` is required so `QuerySet::filter`'s closure can construct
    /// the ZST handle without the caller naming it; the generated struct
    /// trivially satisfies this via `#[derive(Default)]`. The unit type
    /// `()` also satisfies the bound, which keeps pre-Phase-2 test-only
    /// `Model` impls (e.g. the `Fake` model in `query::field`'s unit
    /// tests) valid without dragging in a full field bag.
    type Fields: Copy + Default + Send + Sync + 'static;

    /// SQL table name.
    fn table_name() -> &'static str;

    /// Construct a lazy [`QuerySet<Self>`](crate::query::QuerySet) for this
    /// model. The default impl returns an empty queryset — no filters, no
    /// ordering, no limit — and is correct for every model; individual
    /// models should not override it.
    ///
    /// This method is the canonical entry point for every query, so the
    /// trait's associated `type Fields` bounds (`Copy + Default + Send +
    /// Sync + 'static`) already satisfy `QuerySet::filter`'s default-
    /// constructed field bag.
    ///
    /// Proxy models inherit `objects()` and rely on the default-filter /
    /// default-ordering hooks (Phase 8β T3.4) to seed the returned
    /// queryset with the proxy's `#[model(default_filter, default_order)]`
    /// state — the override happens inside [`crate::query::QuerySet::new`]
    /// rather than here so non-proxy models never pay a virtual-call cost.
    fn objects() -> crate::query::QuerySet<Self> {
        crate::query::QuerySet::new()
    }

    /// Default filter AND-composed into every freshly constructed
    /// [`crate::query::QuerySet<Self>`]. Proxy models override via
    /// `#[model(proxy_for = Parent, default_filter = |f| ...)]` — the
    /// macro emits an override returning `Some(Condition::RawSql(...))`
    /// containing the lowered SQL fragment from
    /// `model::proxy::lower_default_filter_to_sql`.
    ///
    /// Non-proxy models keep this default impl (returns `None`), which
    /// is a zero-cost no-op at every `QuerySet::new()` call site —
    /// rustc inlines the `None` return and the conditional in
    /// [`crate::query::QuerySet::new`] folds the default seed away.
    ///
    /// User `.filter(|f| ...)` calls AND-compose with the default,
    /// matching Django-style semantics: the proxy filter is the prefix
    /// no adopter call can drop, and explicit filters narrow further on
    /// top of it. Bulk-delete on a proxy queryset inherits this scoping
    /// automatically — no separate runtime warning per D5 (v3 line 144).
    ///
    /// # Why `Option<Condition>` rather than `Condition`
    ///
    /// `None` is the structural-no-op signal for the default impl —
    /// distinct from `Some(Condition::True)`. Both render the same SQL,
    /// but `None` lets [`crate::query::QuerySet::new`] short-circuit
    /// without an enum match per call (the cost of which would be
    /// noise on the hot construction path for non-proxy querysets).
    fn default_filter_condition() -> Option<crate::query::internal::Condition> {
        None
    }

    /// Default ordering applied to every freshly constructed
    /// [`crate::query::QuerySet<Self>`]. Proxy models override via
    /// `#[model(proxy_for = Parent, default_order = [(field, Asc), ...])]`
    /// — the macro emits an override returning the lowered
    /// [`Vec<crate::query::OrderExpr>`] from the parsed
    /// `(field, Asc|Desc)` tuples.
    ///
    /// Non-proxy models keep this default impl (returns the empty
    /// `Vec`). User `.order_by(|f| ...)` calls **append** to the
    /// default per the existing Django-style queryset convention
    /// (`queryset.rs` lines 25–28 — append, not replace). Adopter
    /// surprise is minimised: one ordering rule for every queryset
    /// shape, regardless of proxy / non-proxy status.
    ///
    /// # Why `Vec` rather than `&'static [OrderExpr]`
    ///
    /// `OrderExpr` is `Clone` but not `Copy` (the spatial variant
    /// holds a `GeoPoint`), so a `&'static [OrderExpr]` would force
    /// every call site to clone the slice into a `Vec` for the
    /// `QuerySet::ordering` field anyway. Returning the `Vec` here
    /// flattens the call chain and keeps the override path compact for
    /// proxy macro emission.
    fn default_order_by() -> Vec<crate::query::OrderExpr> {
        Vec::new()
    }

    /// Returns the primary key value for this instance.
    fn pk_value(&self) -> &Self::Pk;

    /// Static model descriptor — used by the migration differ (Phase 6).
    fn descriptor() -> &'static ModelDescriptor;

    /// Fetch by primary key. Returns `DjogiError::NotFound` if absent.
    fn get(
        ctx: &mut DjogiContext,
        id: Self::Pk,
    ) -> impl Future<Output = Result<Self, DjogiError>> + Send;

    /// Insert a new row. Framework fields (`id`, `created_at`, `updated_at`)
    /// from `value` are ignored — the database populates them via defaults
    /// and `RETURNING *`.
    fn create(
        ctx: &mut DjogiContext,
        value: Self,
    ) -> impl Future<Output = Result<Self, DjogiError>> + Send;

    /// Update all user-defined fields for this row. Sets `updated_at = now()`.
    ///
    /// On success `self` is rehydrated from the `UPDATE ... RETURNING *`
    /// result — `updated_at` advances, and any column mutated by a
    /// `BEFORE UPDATE` trigger or server-side default surfaces in the
    /// receiver. In-memory state cannot drift from database truth.
    fn save<'ctx>(
        &'ctx mut self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<(), DjogiError>> + Send + 'ctx;

    /// Delete this row.
    fn delete(self, ctx: &mut DjogiContext) -> impl Future<Output = Result<(), DjogiError>> + Send;

    /// Reload this row from the database, returning a fresh instance.
    fn refresh_from_db<'ctx>(
        &'ctx self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Self, DjogiError>> + Send + 'ctx;

    // ── Phase 8.5 djogi#180 — PG18 OLD/NEW RETURNING ─────────────────────────

    /// Update this row and return a before/after snapshot pair.
    ///
    /// Uses PostgreSQL 18 `RETURNING WITH (OLD AS __djogi_old, NEW AS __djogi_new)`
    /// to retrieve both the pre-update and post-update row images in a single
    /// round-trip, decoded into a [`ReturningPair<Self>`].
    ///
    /// # Consumes `self`
    ///
    /// This method consumes `self` because the caller's in-memory instance is
    /// stale after the update. The type system prevents accidental reuse.
    /// Continue working with `pair.new` after the call returns.
    ///
    /// # Relation to `save()`
    ///
    /// `save()` is the in-place update API — it rehydrates `self` from the DB
    /// and returns `()`. Use `save()` when you want to continue using the same
    /// instance. Use `update_returning_pair` when you need both the before and
    /// after snapshots.
    ///
    /// # Versioned models
    ///
    /// For models with `#[field(version)]`, this method enforces the same
    /// optimistic-lock behavior as `save()`: if the DB version has advanced
    /// beyond the in-memory value, the call returns
    /// [`DjogiError::LockConflict`].
    ///
    /// # Hooks and outbox
    ///
    /// Hook and outbox order mirrors `save()`:
    /// `before_save → UPDATE RETURNING → outbox(pair.new) → after_save(pair.new) → on_commit`.
    ///
    /// The outbox `Save` payload is the DB-returned post-image (`pair.new`),
    /// not the (stale) consumed `self`. No diff-shaped outbox payload is
    /// emitted in this release — see the outbox module docs for the v1 policy.
    ///
    /// # Protected fields
    ///
    /// Both `old` and `new` expose full model field values, including
    /// `#[field(protected(...))]` fields. Redaction policy is deferred to
    /// issue #227.
    ///
    /// # PostgreSQL 18 only
    ///
    /// Djogi has a hard PostgreSQL 18 floor. No fallback or polyfill is
    /// provided for older PostgreSQL versions.
    fn update_returning_pair(
        self,
        ctx: &mut DjogiContext,
    ) -> impl Future<Output = Result<crate::query::ReturningPair<Self>, DjogiError>> + Send {
        // The `#[model]` macro emits a real implementation that issues the PG18
        // `UPDATE … RETURNING WITH (OLD AS __djogi_old, NEW AS __djogi_new)`
        // SQL. This default body exists only so hand-rolled test `Fake` models
        // (which are never actually called for returning-pair operations) satisfy
        // the trait without boilerplate — the same pattern used for
        // `__delta_should_tombstone` and `__djogi_emit_field_predicate`. Using
        // `ctx` in the signature keeps the bound parameter visible; suppress the
        // unused-variable lint explicitly.
        let _ = ctx;
        async {
            unreachable!("update_returning_pair: #[model] macro must emit this implementation")
        }
    }

    /// Delete this row and return the pre-delete DB snapshot.
    ///
    /// Uses PostgreSQL 18 `RETURNING WITH (OLD AS __djogi_old)` to retrieve the
    /// row's state at the moment of deletion. The returned `Self` reflects
    /// server-side defaults and trigger effects visible in the `OLD` table
    /// reference — it is more reliable than the consumed `self` for outbox
    /// and audit purposes.
    ///
    /// DELETE has no `NEW` side. For UPDATE before/after pairs see
    /// [`Model::update_returning_pair`].
    ///
    /// # Consumes `self`
    ///
    /// Like [`Model::delete`], this method consumes `self`. The returned value
    /// is the DB-authoritative pre-delete snapshot, not the caller's value.
    ///
    /// # Hooks and outbox
    ///
    /// Hook and outbox order mirrors `delete()`:
    /// `before_delete → DELETE RETURNING → outbox(deleted) → after_delete(deleted) → on_commit`.
    ///
    /// The outbox `Delete` payload is the DB-returned snapshot (`deleted`), not
    /// the consumed `self`. This is more accurate when `BEFORE DELETE` triggers
    /// modify the row before deletion.
    ///
    /// # Protected fields
    ///
    /// The returned snapshot contains full model field values, including
    /// `#[field(protected(...))]` fields. Redaction policy is deferred to
    /// issue #227.
    ///
    /// # PostgreSQL 18 only
    ///
    /// Djogi has a hard PostgreSQL 18 floor. No fallback or polyfill is
    /// provided for older PostgreSQL versions.
    fn delete_returning(
        self,
        ctx: &mut DjogiContext,
    ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
        // Same rationale as `update_returning_pair` above — the `#[model]` macro
        // emits the real PG18 `DELETE … RETURNING WITH (OLD AS __djogi_old)`
        // body. Hand-rolled test stubs get this default.
        let _ = ctx;
        async { unreachable!("delete_returning: #[model] macro must emit this implementation") }
    }

    // ── Tree-recursive sugar (Phase 8-Zero Cluster B2 — T9) ─────────────────
    //
    // These default methods provide a `tree_edge`-aware shorthand for
    // [`crate::query::QuerySet::tree_descendants`] /
    // [`crate::query::QuerySet::tree_ancestors`]. They resolve the
    // self-FK column at runtime from
    // [`ModelDescriptor::tree_edge`](crate::descriptor::ModelDescriptor)
    // and fail with [`DjogiError::Validation`] if the model has not
    // declared `#[model(tree_edge = "...")]`.
    //
    // The runtime check is the deliberate trade-off: a compile-time
    // gate would require either an extra trait the macro implements
    // only when `tree_edge` is set, or generic-bounded specialization
    // (unstable). Pre-1.0 we ship the runtime gate; B5's lihaaf
    // compile-fail fixture covers the type-level error case for the
    // explicit-path API (`QuerySet::tree_descendants` with a mismatched
    // `RelationPath`).

    /// `tree_edge`-aware shorthand for
    /// [`QuerySet::tree_descendants`](crate::query::QuerySet::tree_descendants).
    ///
    /// Resolves the self-FK column from this model's
    /// `#[model(tree_edge = "...")]` declaration and constructs a
    /// [`RecursiveQuerySet`](crate::query::RecursiveQuerySet)
    /// pre-anchored at `root_id`.
    ///
    /// # Errors
    ///
    /// Returns [`DjogiError::Validation`] when the model has not
    /// declared a default `tree_edge`. The error message names the
    /// model and instructs the caller to either add
    /// `#[model(tree_edge = "...")]` or use the explicit-path
    /// [`QuerySet::tree_descendants`] form.
    fn tree_descendants(
        root_id: Self::Pk,
    ) -> Result<crate::query::RecursiveQuerySet<Self>, DjogiError>
    where
        Self::Pk: postgres_types::ToSql + Sync + Send + 'static,
    {
        resolve_tree_edge::<Self>().map(|edge| {
            crate::query::RecursiveQuerySet::from_path(
                edge,
                root_id,
                crate::query::RecursiveDirection::Descendants,
            )
        })
    }

    /// `tree_edge`-aware shorthand for
    /// [`QuerySet::tree_ancestors`](crate::query::QuerySet::tree_ancestors).
    /// Same descriptor lookup + error contract as
    /// [`Model::tree_descendants`].
    fn tree_ancestors(
        node_id: Self::Pk,
    ) -> Result<crate::query::RecursiveQuerySet<Self>, DjogiError>
    where
        Self::Pk: postgres_types::ToSql + Sync + Send + 'static,
    {
        resolve_tree_edge::<Self>().map(|edge| {
            crate::query::RecursiveQuerySet::from_path(
                edge,
                node_id,
                crate::query::RecursiveDirection::Ancestors,
            )
        })
    }

    /// Framework-internal: returns `true` when this row should be tombstoned
    /// (rather than upserted) by the delta-sync fetcher. Default: `false`.
    ///
    /// Models opting into `#[model(soft_deletable)]` get a macro-emitted
    /// override that forwards to
    /// `<Self as SoftDeletable>::deleted_at(self).is_some()`.
    /// Adopters do NOT override this method directly — the soft-delete
    /// surface is `#[model(soft_deletable)]` + the `SoftDeletable` trait.
    ///
    /// # Why on `Model` rather than gated on `SoftDeletable`
    ///
    /// The delta-sync fetcher in `djogi::query::refresh` walks items
    /// generically over `T: Model + Cacheable + ...`. Rust's coherence rules
    /// don't allow specializing the walk based on whether `T: SoftDeletable`,
    /// so the soft-delete signal lives on `Model` with a default `false`.
    /// Non-soft-delete models pay zero (the default is a constant), and the
    /// runtime check is a single virtual call per row — negligible vs the
    /// SQL round-trip.
    #[doc(hidden)]
    fn __delta_should_tombstone(&self) -> bool {
        false
    }

    /// Framework-internal: emit a portable field predicate as SQL.
    ///
    /// Phase 8eta PR2a installs the default that returns
    /// [`crate::query::PortablePredicateError::UnsupportedModel`] so
    /// hand-written `Model` impls (test stubs, internal fixtures with
    /// `type Fields = ()`) keep compiling without claiming to support
    /// portable SQL lowering. PR2d's macro override replaces this default
    /// on every PK-backed `#[model]`-emitted impl with a generated
    /// `(field_name, LookupOp)` dispatch that calls into the
    /// `crate::query::portable::emit::*` helpers.
    ///
    /// # Why on `Model` rather than a separate trait?
    ///
    /// The direct-`Q<T>` walker in PR2b iterates `Q::Portable` leaves
    /// generically over `T: Model`. A separate `PortableSqlEmit<T>` trait
    /// would require every existing test fixture and adopter `Model` impl
    /// to add a stub implementation before PR2b could compile — PRs
    /// would not be bisectable. Putting the hook on `Model` with a safe
    /// default keeps the trait surface unified and preserves bisection.
    ///
    /// # Adopter contract
    ///
    /// Adopters do **not** override this method directly — the `#[model(...)]`
    /// attribute macro emits the override on every macro-generated impl.
    /// Hand-written `impl Model` blocks (which are themselves discouraged
    /// outside of internal test fixtures because `__sealed::Sealed` is
    /// private) keep the default and surface a typed error if a portable
    /// predicate against the model ever reaches SQL emission.
    #[doc(hidden)]
    fn __djogi_emit_field_predicate(
        acc: &mut crate::pg::accumulator::SqlAccumulator,
        field: &crate::types::FieldPredicate<Self>,
        ctx: crate::query::SqlEmitContext,
    ) -> Result<(), crate::query::PortablePredicateError> {
        let _ = (acc, field, ctx);
        Err(crate::query::PortablePredicateError::UnsupportedModel {
            model: ::core::any::type_name::<Self>(),
        })
    }

    /// Walk **every** self-FK edge declared on this model upward —
    /// the multi-edge sibling of [`Model::tree_ancestors`]. Phase
    /// 8-Zero Cluster B3 (T13a).
    ///
    /// `full_ancestors` is the right shape for kinship / pedigree
    /// queries where a node has more than one parent edge (e.g.
    /// `mother_id` + `father_id` on an animal model). The recursive
    /// CTE emits a single recursive SELECT that fans the per-edge
    /// alternatives out through a non-recursive `JOIN LATERAL (...
    /// UNION ALL ...) child ON TRUE` subquery, so a single call
    /// returns ancestors reachable via any combination of those
    /// edges. Path multiplicity is preserved — an ancestor reachable
    /// by two distinct edge sequences appears twice, which is
    /// load-bearing for Wright-style kinship coefficient sums.
    ///
    /// Combine with
    /// [`fetch_all_with_paths`](crate::query::RecursiveQuerySet::fetch_all_with_paths)
    /// to recover which edge sequence reached each ancestor — that is
    /// the only terminal that distinguishes
    /// `["mother_id", "father_id"]` from `["father_id", "mother_id"]`
    /// when both lead to the same row.
    ///
    /// # Edge cases
    ///
    /// - `self_fk_count() == 0` — the returned `RecursiveQuerySet`
    ///   carries an empty `edges` Vec. Builder methods chain
    ///   normally; the **terminal** fails with
    ///   [`DjogiError::Validation`] naming the model. Errors at
    ///   terminal time (not construction time) keep the return type
    ///   uniform — callers can write `Model::full_ancestors(id)
    ///   .with_max_depth(5).fetch_all(ctx).await?` without an extra
    ///   `?` for `self_fk_count() == 0`.
    /// - `self_fk_count() == 1` — degenerates to
    ///   [`Model::tree_ancestors`] over the lone edge. Same SQL
    ///   shape, same single bind for the root id.
    /// - `self_fk_count() >= 2` — every declared self-FK becomes its
    ///   own alternative inside the lateral `UNION ALL` subquery the
    ///   recursive term joins to (Postgres restricts recursive CTEs
    ///   to one self-reference, so the per-edge fan-out lives in a
    ///   non-recursive lateral). No `tree_edge` requirement:
    ///   `full_ancestors` is the disambiguation strategy, not
    ///   single-edge selection.
    fn full_ancestors(node_id: Self::Pk) -> crate::query::RecursiveQuerySet<Self>
    where
        Self::Pk: postgres_types::ToSql + Sync + Send + 'static,
    {
        let descriptor = Self::descriptor();
        let edges: Vec<crate::relation::RelationPath<Self, Self>> = descriptor
            .self_fk_columns()
            .map(|col| {
                crate::relation::__macro_support::__make_relation_path::<Self, Self>(
                    col,
                    Self::table_name(),
                    crate::relation::RelationKind::ForeignKey,
                )
            })
            .collect();
        crate::query::RecursiveQuerySet::from_paths(
            edges,
            node_id,
            crate::query::RecursiveDirection::Ancestors,
        )
    }

    // ── Materialised transitive closure (Phase 8-Zero Cluster B4 — T13b) ─────
    //
    // [`materialize_closure`](Model::materialize_closure) populates a
    // closure-table sibling of this model. Per the scalability lens
    // (Risk 10), materialised transitive closure is the production-
    // scale answer for tree queries: every adopter doing tree
    // queries at non-trivial scale eventually reaches for one, and
    // shipping a framework helper means the framework *supports* the
    // production pattern rather than just *demonstrating* the
    // recursive-CTE one.
    //
    // The macro-side wiring (`#[model(closure_for = T)]`) that would
    // generate the [`ClosureModel`] impl from a single attribute is
    // explicitly out of scope for B4 — adopters hand-write the impl
    // for now. Runtime contract is fixed; macro sugar can land later
    // without changing this method signature.

    /// Populate a transitive-closure table for this model's self-FK
    /// graph. Phase 8-Zero Cluster B4 (T13b).
    ///
    /// `C` is an adopter-supplied [`ClosureModel`] whose `Source =
    /// Self` — the type-level binding pins the closure table to the
    /// source model so wrong-source closure tables fail at compile
    /// time. Reach for this helper when:
    ///
    /// - The source table has more than a handful of rows and tree
    ///   queries against it have become hot. Closure-table lookups
    ///   are indexed point-reads; recursive-CTE walks are O(subtree
    ///   size) every time.
    /// - The application needs Wright-style kinship coefficients.
    ///   The closure table records `path_count` per
    ///   `(source, ancestor, depth)` triple, which is the input to
    ///   coefficient sums.
    ///
    /// # Behaviour
    ///
    /// - **`opts.roots = None`** — walks every row in the source
    ///   table. Right shape for the initial population.
    /// - **`opts.roots = Some(ids)`** — walks only those source rows.
    ///   Right shape for incremental updates after inserts (call with
    ///   the newly-inserted ids).
    /// - **`opts.max_depth = Some(n)`** — bounds the recursive walk
    ///   at `n` hops. `None` runs to natural exhaustion (the `CYCLE`
    ///   clause prevents infinite recursion regardless).
    /// - **`ON CONFLICT … DO UPDATE`** — replaces `path_count` with
    ///   the recomputed recursive-walk total, so re-running the helper
    ///   is genuinely idempotent: each invocation walks the current
    ///   graph from scratch, so EXCLUDED's count is already the
    ///   correct total. `TRUNCATE` the closure table first only when
    ///   stale rows for *deleted* edges must be purged (the helper
    ///   does not garbage-collect rows whose path no longer exists).
    ///
    /// # Required closure-table schema
    ///
    /// The closure table **must** carry a unique constraint on
    /// `(source_column, ancestor_column, depth_column)` — Postgres
    /// rejects `ON CONFLICT (...)` against missing constraints with
    /// `42P10`. See [`crate::query::closure`] module docs for the
    /// canonical CREATE TABLE shape.
    ///
    /// # Errors
    ///
    /// Returns [`DjogiError::Validation`] when:
    ///
    /// - `Self::descriptor().self_fk_count() == 0` — there are no
    ///   self-FK edges to walk.
    /// - Any [`ClosureModel`] column-name accessor returns an invalid
    ///   identifier (non-ASCII, reserved keyword, > 63 bytes, etc.).
    ///
    /// Returns the underlying database error wrapped in
    /// [`DjogiError`] for query failures (typically a missing unique
    /// constraint surfaces as `42P10` here).
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Initial population — walk every row.
    /// let report = Elephant::materialize_closure::<ElephantAncestry>(
    ///     &mut ctx,
    ///     MaterializeClosureOptions::default(),
    /// ).await?;
    /// println!("populated {} triples across {} elephants",
    ///          report.rows_written, report.sources_visited);
    ///
    /// // Incremental update for newly-inserted elephants.
    /// let new_ids: Vec<HeerId> = /* ... */;
    /// let report = Elephant::materialize_closure::<ElephantAncestry>(
    ///     &mut ctx,
    ///     MaterializeClosureOptions::default()
    ///         .with_roots(new_ids)
    ///         .with_max_depth(20),
    /// ).await?;
    /// ```
    fn materialize_closure<'ctx, C>(
        ctx: &'ctx mut DjogiContext,
        opts: crate::query::MaterializeClosureOptions<Self::Pk>,
    ) -> impl Future<Output = Result<crate::query::MaterializeClosureReport, DjogiError>> + Send + 'ctx
    where
        C: crate::query::ClosureModel<Source = Self> + 'ctx,
        Self::Pk: postgres_types::ToSql + Sync + Send + 'static,
    {
        crate::query::closure::materialize_closure_impl::<Self, C>(ctx, opts)
    }
}

/// Look up `M`'s declared `tree_edge` and synthesise a
/// `RelationPath<M, M>` that targets the same model's table.
///
/// The descriptor's `tree_edge` is the field NAME (which equals the
/// column name in Djogi); the macro's compile-time validation in B1
/// (T12) already proved both that the named field exists on the
/// struct and that it is a self-FK, so the lookup here is a pure
/// metadata read with no fallible step beyond the
/// `tree_edge.is_some()` check.
///
/// `target_table = M::table_name()` because a self-FK by definition
/// targets the same model. `RelationKind::ForeignKey` is the
/// canonical kind for self-FK edges; if a future phase adds
/// `OneToOne` self-FKs the descriptor's `relation_kind` field would
/// carry the right discriminant and a richer lookup could thread it
/// through, but for B2 the kind is informational only — the SQL
/// emitter treats both kinds identically (single FK column → one
/// recursive walk).
fn resolve_tree_edge<M: Model>() -> Result<crate::relation::RelationPath<M, M>, DjogiError> {
    let descriptor = M::descriptor();
    let edge_name = descriptor.tree_edge.ok_or_else(|| {
        DjogiError::Validation(format!(
            "model '{}' has no #[model(tree_edge = \"...\")] declared; \
             either add the attribute or use QuerySet::tree_descendants / \
             QuerySet::tree_ancestors with an explicit RelationPath",
            descriptor.type_name,
        ))
    })?;
    Ok(
        crate::relation::__macro_support::__make_relation_path::<M, M>(
            edge_name,
            M::table_name(),
            crate::relation::RelationKind::ForeignKey,
        ),
    )
}
