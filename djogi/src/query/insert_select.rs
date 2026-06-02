//! Bulk `INSERT INTO target (cols...) SELECT exprs... FROM source [WHERE ...]`
//! copy rows from one model's queryset into another model's table.
//! Closes [](https://github.com/TarunvirBains/djogi/issues/106).
//! # What
//! [`QuerySet::insert_into`] (defined in this module) consumes a source
//! [`QuerySet<S>`] and a typed
//! `|T::Fields, S::Fields| -> Vec<InsertSelectColumn<S, T>>` closure, returning
//! an inert [`InsertSelectStmt<S, T>`] that runs when the caller invokes
//! [`InsertSelectStmt::execute`] with a `&mut DjogiContext`. The terminal
//! returns the affected row count.
//! [`FieldRef::copy_from`] is the typed column-mapping constructor — but
//! it is only callable on a target-side field, and accepts only a
//! source-side operand. The source-side operand is the
//! [`InsertSelectSource<S, V>`] type built from a source field via
//! [`FieldRef::as_insert_source`] / [`crate::query::DjogiField::as_insert_source`]
//! or from a Rust scalar via [`InsertSelectSource::literal`]. Both the
//! target column's `V` and the source operand's `V` are matched at compile
//! time, AND the source model `S` is pinned to the closure's source bag via
//! the closure's return type — passing a target-side value where a
//! source-side operand is required (or vice-versa) is rejected by the type
//! system, not the runtime emitter.
//! # Why
//! Adopters who need to copy rows from one query result into another
//! table (archival "copy-then-delete" patterns, bulk migrations between
//! tables, data-pipeline stages) had to drop to `ctx.raw_execute(...)`
//! before this module — the same shape that motivates the
//! "raw SQL is djogi's `unsafe`" cultural framing in `CLAUDE.md`. Closing
//! the gap moves the pattern out of the bypass attribute and into the
//! typed surface.
//! # How
//! ```ignore
//! use djogi::prelude::*;
//!
//! // Archive completed orders into an archive table. The `_, _` placeholders
//! // are the closure type and the return-shape type — Rust infers both, but
//! // the target model `T` is the one type parameter the call site must name
//! // because nothing else carries it (the closure is `FnOnce` over both
//! // field bags, so neither argument's type can supply `T`).
//! CompletedOrder::objects()
//!     .filter(|f| f.completed_at().lt(cutoff))
//!     .insert_into::<OrderArchive, _, _>(|target, source| vec![
//!         target.order_id().copy_from(source.id().as_insert_source()),
//!         target.title().copy_from(source.title().as_insert_source()),
//!         target.completed_at().copy_from(source.completed_at().as_insert_source()),
//!     ])
//!     .execute(&mut ctx)
//!     .await?;
//! ```
//! # What this surface does NOT cover
//! - **Set operations** (`UNION` / `INTERSECT` / `EXCEPT`) — tracked in
//!   .
//! - **`LATERAL` joins** — tracked in .
//! - **`VALUES` inline relations as join sources** — tracked in .
//! - **PG18 `OLD` / `NEW` in `RETURNING`** — tracked in .
//! - **`RETURNING` for INSERT...SELECT** — use
//!   [`InsertSelectStmt::execute_returning`] to receive every inserted
//!   row back as a decoded `Vec<T>` (closes ).
//!   [`InsertSelectStmt::execute`] remains the default for bulk operations
//!   where materialising the full result set is unnecessary.
//! # Framework-column semantics
//! The target's framework columns (`id`, `created_at`, `updated_at`)
//! are populated by their column-level `DEFAULT` clauses on the target
//! table — the emitter never names them in the INSERT column list nor
//! the SELECT projection unless the adopter explicitly maps them in the
//! closure. This matches [`Model::create`]'s contract: framework fields
//! supplied by the caller are ignored; the database populates them.
//! Adopters who want to copy the source's `id` into the target (e.g. to
//! preserve the original identity on an archive table) explicitly map it
//! via `target.original_id().copy_from(source.id().as_insert_source())`
//! against an adopter-declared user column. Adopters who name
//! `target.id()` directly against an FK-typed PK column take responsibility
//! for the resulting collision behaviour just as they would in
//! `Model::create_with_id`.
//! # Source/target identity is type-checked
//! The typed surface refuses to compile when the source and target sides
//! are swapped at the mapping site. Concretely:
//! - `target_field.copy_from(source_field.as_insert_source())` — OK.
//! - `source_field.copy_from(target_field.as_insert_source())` — fails
//!   to compile (`InsertSelectColumn<T, S>` does not implement
//!   `IntoInsertColumns<S, T>`).
//! - `target_field.copy_from(target_field.as_insert_source())` — fails
//!   to compile (`InsertSelectColumn<T, T>` does not implement
//!   `IntoInsertColumns<S, T>` when `S != T`).
//!   See `djogi/tests/compile_fail/insert_select_*` for the pinned
//!   compile-fail fixtures.
//! # Rejected source state
//! [`InsertSelectStmt::execute`] returns [`DjogiError::Validation`] when
//! the source queryset carries state that cannot be safely represented
//! in an INSERT...SELECT shape:
//! - **`prefetch_paths`** — prefetch is a post-fetch row-stitching
//!   pattern; INSERT...SELECT returns no rows, so prefetch has no
//!   meaning.
//! - **`select_related_paths`** — select_related expands the SELECT list
//!   with aliased joined columns; the column mapping closure references
//!   single-model columns, so silently dropping the join would surprise
//!   the caller. Rejecting forces the caller to compose the joined
//!   source via an explicit subquery in a future iteration.
//! - **`cache_target`** — `.cache(&p)` writes returned rows into a
//!   Punnu; INSERT...SELECT returns row counts, not rows. Rejecting
//!   surfaces the bug rather than silently dropping the cache binding.
//! - **`lock != LockMode::None`** — `SELECT ... FOR UPDATE` or
//!   `... FOR SHARE` inside an INSERT...SELECT is a legitimate
//!   Postgres pattern (acquire the source rows' locks for the
//!   duration of the archival), but the minimum coherent surface for
//!   ships without lock composition. The FOR SHARE family
//!   added under is rejected by the same validator. A future
//!   issue can lift this restriction with a deliberate
//!   `.with_source_lock()` opt-in.
//! - **`distinct != DistinctMode::None`** — `SELECT DISTINCT` inside
//!   INSERT...SELECT is also valid Postgres semantics ("insert distinct
//!   source rows only"), but the safer initial surface rejects it.
//!   `DistinctMode::On(cols)` in particular requires the DISTINCT ON
//!   columns to appear at the start of `ORDER BY` AND in the SELECT
//!   projection — neither of which the closure-built source-expression
//!   list guarantees. Rejecting all non-default distinct modes keeps
//!   the surface obviously correct.
//! # Allowed source state
//! - **`condition`** (WHERE clause) — the canonical filter surface.
//! - **`ordering`** (ORDER BY) — composes with LIMIT to deterministically
//!   pick "oldest N" / "newest N" subsets. Postgres preserves the order
//!   into the INSERT.
//! - **`limit`** (LIMIT) — useful for chunked archival.
//! - **`offset`** (OFFSET) — composes with limit for pagination-style
//!   chunking.
//! - **`is_empty`** (the `QuerySet::none()` short-circuit) — terminals
//!   return `Ok(0)` without touching the database, **but only after**
//!   the column-mapping and source-state validation above has passed.
//!   A `.none()` chain with an empty mapping, duplicate columns, or
//!   stale post-`.none()` state-adding methods still surfaces a
//!   [`DjogiError::Validation`] so the programming error does not hide
//!   behind a silent zero-row return.
//! # Tenant / RLS auto-set
//! [`InsertSelectStmt::execute`] calls `auto_set_tenant::<T>(ctx)` for
//! the target model before issuing the INSERT, then `auto_set_tenant::<S>(ctx)`
//! for the source — both calls are idempotent (the helper checks
//! `applied_tenant_id`), so the second call is a no-op when target and
//! source share the same tenant key (the typical multi-tenant case).
//! Cross-tenant INSERT...SELECT is out of scope; the caller is expected
//! to manage the auth context explicitly when copying across tenants.
//! # Duplicate column rejection
//! The emitter rejects an empty column list (Postgres would reject
//! `INSERT INTO t () SELECT ...` regardless) and a column list with
//! duplicate target columns (Postgres would reject
//! `INSERT INTO t (a, a) SELECT ...` with `42701 column "a" specified
//! more than once`). Both are surfaced as [`DjogiError::Validation`]
//! before the SQL leaves the framework, so the diagnostic carries the
//! target table name rather than the bare Postgres SQLSTATE.

#![allow(clippy::manual_async_fn)]

use crate::DjogiError;
use crate::context::DjogiContext;
use crate::expr::Expr;
use crate::expr::arithmetic::Numeric;
use crate::expr::node::ExprNode;
use crate::model::Model;
use crate::pg::accumulator::as_params;
use crate::pg::decode::FromPgRow;
use crate::query::field::FieldRef;
use crate::query::queryset::QuerySet;
use crate::query::sql::{build_insert_select, build_insert_select_returning};
use crate::query::terminal::auto_set_tenant;
use std::collections::HashSet;
use std::future::Future;
use std::marker::PhantomData;

/// Source-tagged expression operand for [`FieldRef::copy_from`] /
/// [`crate::query::DjogiField::copy_from`].
/// # What
/// `InsertSelectSource<S, V>` wraps the source-side projection IR for one
/// position in an `INSERT INTO ... SELECT ...` mapping. The phantom `S`
/// pins the operand to a specific source model so the mapping cannot
/// silently cross model boundaries; the phantom `V` pins the value type
/// so a target column of type `V` can only be fed by a source operand of
/// the matching `V`.
/// # How to construct
/// - **From a source field** — call
///   [`FieldRef::as_insert_source`] / [`crate::query::DjogiField::as_insert_source`].
///   The receiver's `M` pins the wrapper's `S`, so building an operand
///   from a target field gives an operand tagged with the target model
///   which the closure return type then rejects.
/// - **From a Rust scalar** — call [`InsertSelectSource::literal`]. `S`
///   is free at construction and inferred from the closure context (a
///   constant has no source identity of its own).
/// - **From an arithmetic composition** — use `+` / `-` / `*` / `/` on
///   `InsertSelectSource<S, V>` where `V: Numeric`. Same-`S` operands
///   compose; mixing two different source tags fails to compile.
/// # Why phantom-only
/// The IR payload (the crate-private `ExprNode` enum) is type-erased at
/// the leaf — the SQL emitter walks one monomorphic function regardless
/// of `S` or `V`. `S` and `V` are present strictly to drive compile-time
/// checks at the mapping site; they never appear in the rendered SQL
/// or in the `SqlAccumulator`'s bind list. Cf. the parallel design on
/// [`crate::expr::Expr<V>`] — same rationale.
/// `Debug` + `Clone` are derived because the inner IR tree is both.
/// `Copy` is intentionally NOT implemented — the IR contains boxed
/// sub-nodes, so cheap-looking `Copy` would hide allocation costs in
/// arithmetic chains.
#[must_use = "InsertSelectSource is lazy — drop one and the source projection is silently omitted"]
#[derive(Debug, Clone)]
pub struct InsertSelectSource<S: Model, V> {
    pub(crate) node: ExprNode,
    _phantom: PhantomData<fn() -> (S, V)>,
}

impl<S: Model, V> InsertSelectSource<S, V> {
    /// Build a literal source operand from a Rust scalar.
    /// The `S` parameter is polymorphic and inferred from the closure
    /// context — a literal has no source identity of its own, so any
    /// `S` satisfies the construction site. The closure's return-type
    /// inference then pins `S` to the source model the enclosing
    /// `QuerySet<S>::insert_into` call uses.
    /// # Example
    /// ```ignore
    /// // Every archived row gets the same constant `status = "ARCHIVED"`.
    /// .insert_into::<OrderArchive, _, _>(|target, _source| vec![
    ///     target.status().copy_from(InsertSelectSource::literal("ARCHIVED".to_string())),
    /// ])
    /// ```
    /// `V: Into<Expr<V>>` is satisfied by every scalar Djogi binds today
    /// (the crate-private `crate::expr::literal` module is the source of
    /// truth for the bindable set); the `Into` conversion routes through
    /// the same typed seal that [`Expr::literal`] uses.
    #[must_use = "InsertSelectSource is lazy — drop one and the source projection is silently omitted"]
    pub fn literal(v: V) -> Self
    where
        V: Into<Expr<V>>,
    {
        let expr: Expr<V> = v.into();
        Self {
            node: expr.node,
            _phantom: PhantomData,
        }
    }

    /// Package an already-constructed [`ExprNode`] into a tagged
    /// `InsertSelectSource<S, V>`. Crate-private so downstream code
    /// cannot fabricate a wrong-`S`-tagged source operand by bypassing
    /// the typed constructors ([`FieldRef::as_insert_source`] and
    /// [`InsertSelectSource::literal`]).
    /// Used internally by the arithmetic operator overloads on
    /// [`InsertSelectSource`] to wrap the freshly-built node without
    /// repeating the `PhantomData` boilerplate at every call site.
    pub(crate) fn from_node(node: ExprNode) -> Self {
        Self {
            node,
            _phantom: PhantomData,
        }
    }
}

// ── Arithmetic operator overloads on `InsertSelectSource<S, V>` ──────────────
// Mirror the typed arithmetic surface on [`crate::expr::Expr<V>`]
// same-`S` and same-`V` composition produces same-`S` and same-`V`
// output. Different `S` tags do not compose (the impl bounds reject
// the mix at compile time), matching the soundness invariant we
// established for the column-mapping site itself.
// The Numeric bound is the sealed marker from `crate::expr::arithmetic`
// same set of accepted scalar types as `Expr<V>` arithmetic
// (`i16`/`i32`/`i64`/`f32`/`f64`/`time::Duration`).

impl<S: Model, V: Numeric> std::ops::Add for InsertSelectSource<S, V> {
    type Output = InsertSelectSource<S, V>;

    fn add(self, rhs: Self) -> Self::Output {
        InsertSelectSource::from_node(ExprNode::Add(Box::new(self.node), Box::new(rhs.node)))
    }
}

impl<S: Model, V: Numeric> std::ops::Sub for InsertSelectSource<S, V> {
    type Output = InsertSelectSource<S, V>;

    fn sub(self, rhs: Self) -> Self::Output {
        InsertSelectSource::from_node(ExprNode::Sub(Box::new(self.node), Box::new(rhs.node)))
    }
}

impl<S: Model, V: Numeric> std::ops::Mul for InsertSelectSource<S, V> {
    type Output = InsertSelectSource<S, V>;

    fn mul(self, rhs: Self) -> Self::Output {
        InsertSelectSource::from_node(ExprNode::Mul(Box::new(self.node), Box::new(rhs.node)))
    }
}

impl<S: Model, V: Numeric> std::ops::Div for InsertSelectSource<S, V> {
    type Output = InsertSelectSource<S, V>;

    fn div(self, rhs: Self) -> Self::Output {
        InsertSelectSource::from_node(ExprNode::Div(Box::new(self.node), Box::new(rhs.node)))
    }
}

// Heterogeneous datetime + interval arithmetic — `OffsetDateTime + Duration`
// produces `OffsetDateTime`. Mirrors the matching impl on `Expr<T>` in
// `crate::expr::arithmetic`. Same-source-tag constraint is the same as the
// same-type arithmetic above.

impl<S: Model> std::ops::Add<InsertSelectSource<S, time::Duration>>
    for InsertSelectSource<S, time::OffsetDateTime>
{
    type Output = InsertSelectSource<S, time::OffsetDateTime>;

    fn add(self, rhs: InsertSelectSource<S, time::Duration>) -> Self::Output {
        InsertSelectSource::from_node(ExprNode::Add(Box::new(self.node), Box::new(rhs.node)))
    }
}

impl<S: Model> std::ops::Sub<InsertSelectSource<S, time::Duration>>
    for InsertSelectSource<S, time::OffsetDateTime>
{
    type Output = InsertSelectSource<S, time::OffsetDateTime>;

    fn sub(self, rhs: InsertSelectSource<S, time::Duration>) -> Self::Output {
        InsertSelectSource::from_node(ExprNode::Sub(Box::new(self.node), Box::new(rhs.node)))
    }
}

/// Source-side construction — promote a source [`FieldRef`] into an
/// [`InsertSelectSource`] tagged with the same source model.
/// Mirrors [`FieldRef::as_expr`] but pins the source model `S` so the
/// resulting operand can only land in an INSERT...SELECT mapping whose
/// closure return type names the same `S`.
/// # Example
/// ```ignore
/// .insert_into::<OrderArchive, _, _>(|target, source| vec![
///     // `source.id()` is `FieldRef<CompletedOrder, HeerIdDesc>`;
///     // `as_insert_source()` lifts it to `InsertSelectSource<CompletedOrder, HeerIdDesc>`.
///     target.original_id().copy_from(source.id().as_insert_source()),
/// ])
/// ```
/// Calling this on a target-side field (e.g. `target.col().as_insert_source()`)
/// produces an `InsertSelectSource<TargetModel, V>` — which the closure's
/// return type then rejects because it expects an
/// `InsertSelectSource<SourceModel, V>` to land in `InsertSelectColumn<SourceModel, T>`.
impl<S: Model, V> FieldRef<S, V> {
    /// Lift this source-side column reference into a tagged
    /// [`InsertSelectSource<S, V>`] for use inside [`copy_from`].
    /// [`copy_from`]: FieldRef::copy_from
    #[must_use = "InsertSelectSource is lazy — drop one and the source projection is silently omitted"]
    pub fn as_insert_source(self) -> InsertSelectSource<S, V> {
        InsertSelectSource::from_node(ExprNode::Field {
            column: self.column(),
        })
    }
}

/// A single `(target_column, source_expression)` mapping that becomes
/// one position in the `INSERT (...) SELECT ...` shape.
/// # What
/// Produced exclusively by [`FieldRef::copy_from`] /
/// [`crate::query::DjogiField::copy_from`] — the closure call
/// `target.col().copy_from(source.col().as_insert_source())` returns a
/// single `InsertSelectColumn<S, T>` with `target_column = "col"` (the
/// macro-baked column name from the target [`FieldRef`]) and `source =`
/// `source.col`'s tagged IR tree.
/// # Type parameters
/// - `S` — the source model. Pinned by the source operand
///   ([`InsertSelectSource<S, V>`]). When the closure returns
///   `Vec<InsertSelectColumn<S, T>>`, `S` must match the
///   `QuerySet<S>::insert_into` receiver's source model.
/// - `T` — the target model. Pinned by the target [`FieldRef<T, V>`] the
///   `copy_from` method is called on. When the closure returns
///   `Vec<InsertSelectColumn<S, T>>`, `T` must match the
///   `insert_into::<T, _, _>` generic on the call site.
///   Mismatch on either side fails to compile at the closure-return
///   inference step — see the module docs and the compile-fail fixtures
///   under `djogi/tests/compile_fail/insert_select_*`.
/// # Invariants
/// - `target_column` is a `&'static str` baked by the `#[model]` macro,
///   never user input — it flows straight into `SqlAccumulator::push_sql`.
/// - `source` is the crate-private `ExprNode` tree built through the
///   typed constructors on [`InsertSelectSource<S, V>`]. The leaf
///   `Field` variant carries a macro-validated `&'static str` column
///   name; the `Literal` variant binds through `push_filter_value` →
///   `push_bind`.
/// - The compile-time `V` matching on [`FieldRef::copy_from`] guarantees
///   the target column's value type matches the source operand's value
///   type at compile time — there is no runtime type-coercion surface
///   here.
///   `Debug` + `Clone` are derived — see the `InsertSelectStmt: Clone`
///   rationale on [`InsertSelectStmt`].
#[must_use = "column mappings are lazy — drop one and the INSERT silently omits the column"]
pub struct InsertSelectColumn<S: Model, T: Model> {
    /// Target column name — macro-baked literal, never user input.
    pub(crate) target_column: &'static str,
    /// Source-side expression IR — emitted via the shared
    /// `crate::expr::sql::emit_expr` walker. The source's table name
    /// is supplied by the upstream `QuerySet<S>` at SQL emission time;
    /// this node only carries the projection.
    pub(crate) source: ExprNode,
    /// Phantom tag pinning the source and target model identity at
    /// compile time without owning either. `fn() -> (S, T)` matches
    /// the variance pattern on [`QuerySet<S>`] and
    /// [`crate::query::UpdateStmt`].
    _phantom: PhantomData<fn() -> (S, T)>,
}

impl<S: Model, T: Model> std::fmt::Debug for InsertSelectColumn<S, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InsertSelectColumn")
            .field("source_table", &S::table_name())
            .field("target_table", &T::table_name())
            .field("target_column", &self.target_column)
            .field("source", &self.source)
            .finish()
    }
}

// `Clone` is hand-rolled, not derived: deriving would impose `S: Clone`
// and `T: Clone` (because the `PhantomData<fn() -> (S, T)>` field appears
// to "own" the type parameters from `derive(Clone)`'s perspective). The
// manual impl mirrors the [`InsertSelectStmt`] pattern below and matches
// the same workaround applied to [`QuerySet<T>`].
impl<S: Model, T: Model> Clone for InsertSelectColumn<S, T> {
    fn clone(&self) -> Self {
        InsertSelectColumn {
            target_column: self.target_column,
            source: self.source.clone(),
            _phantom: PhantomData,
        }
    }
}

impl<S: Model, T: Model> InsertSelectColumn<S, T> {
    /// Internal accessor for the target column name. Used by the SQL
    /// emitter (`crate::query::sql::build_insert_select`) to render the
    /// `INSERT (col1, col2, ...)` list.
    #[doc(hidden)]
    pub fn target_column(&self) -> &'static str {
        self.target_column
    }

    /// Internal accessor for the source expression IR. Used by the SQL
    /// emitter to render the `SELECT expr1, expr2, ...` projection.
    #[doc(hidden)]
    pub(crate) fn source(&self) -> &ExprNode {
        &self.source
    }
}

/// Typed constructor — `target_col.copy_from(source_operand)` produces a
/// single [`InsertSelectColumn<S, T>`] for the closure passed to
/// [`QuerySet::insert_into`].
/// The compile-time `V` is shared between the target column and the
/// source operand — a mismatch (target is `FieldRef<T, String>`,
/// source is `InsertSelectSource<S, i32>`) fails to compile at the call
/// site rather than producing a runtime Postgres "column type mismatch"
/// surface.
/// The compile-time `S` is pinned by the source operand and surfaced on
/// the returned `InsertSelectColumn<S, T>` — together with the closure's
/// return-type inference, that pins both the source and target model
/// identity at the mapping site.
/// # Source operand shapes
/// [`InsertSelectSource<S, V>`] covers every shape the source operand
/// can take:
/// - **Plain column copy** — `source.col().as_insert_source()` (the
///   common case). Emits a bare `col` reference inside the source
///   `FROM` scope.
/// - **Constant** — `InsertSelectSource::literal(42i32)`. Emits a single
///   bind.
/// - **Computed expression** — arithmetic (`+` / `-` / `*` / `/`) on
///   `InsertSelectSource<S, V: Numeric>` composes same-source operands
///   into a single source projection.
/// # Example
/// ```ignore
/// CompletedOrder::objects()
///     .filter(|f| f.completed_at().lt(cutoff))
///     .insert_into::<OrderArchive, _, _>(|target, source| vec![
///         // Column-to-column copy.
///         target.order_id().copy_from(source.id().as_insert_source()),
///         // Compose with arithmetic — bump every score by 1 at archive time.
///         target.score().copy_from(
///             source.score().as_insert_source() + InsertSelectSource::literal(1i32),
///         ),
///         // Constant — every archived row carries the same status.
///         target.status().copy_from(InsertSelectSource::literal("ARCHIVED".to_string())),
///     ])
///     .execute(&mut ctx)
///     .await?;
/// ```
impl<T: Model, V> FieldRef<T, V> {
    /// Bind this target column to a source-tagged operand for an
    /// `INSERT INTO ... SELECT ...` statement.
    /// `V` must match between target and source — the type system pins
    /// the column types in lockstep at compile time. `S` is pinned by
    /// the source operand and propagated into the returned
    /// [`InsertSelectColumn<S, T>`]; closure-return inference then ties
    /// `S` to the enclosing [`QuerySet<S>::insert_into`] receiver, so a
    /// mismatched source identity is rejected by the type system at the
    /// closure boundary.
    #[must_use = "column mappings are lazy — drop one and the INSERT silently omits the column"]
    pub fn copy_from<S: Model>(self, source: InsertSelectSource<S, V>) -> InsertSelectColumn<S, T> {
        InsertSelectColumn {
            target_column: self.column(),
            source: source.node,
            _phantom: PhantomData,
        }
    }
}

/// Closure-return shape for [`QuerySet::insert_into`]. The closure can
/// return either a single [`InsertSelectColumn<S, T>`] or a
/// `Vec<InsertSelectColumn<S, T>>` — this trait bridges both so the user
/// writes the natural thing at the call site.
/// The `<S, T>` parameters are the trait's discriminator: a single
/// closure-return type implements `IntoInsertColumns<S, T>` for exactly
/// one `(S, T)` pair, so the closure's inferred return type ties the
/// source and target identity into the `QuerySet<S>::insert_into::<T, _, _>`
/// receiver. Wrong-side mappings (e.g. returning
/// `Vec<InsertSelectColumn<T, S>>` where `S != T`) do not implement
/// `IntoInsertColumns<S, T>` and fail to compile.
/// Sealed-by-convention: only the two shipped impls
/// (`InsertSelectColumn<S, T>` and `Vec<InsertSelectColumn<S, T>>`)
/// exist, and there is no public trait method that a downstream impl
/// would add value beyond. Users do not implement this trait by hand.
/// Mirrors the [`crate::query::IntoAssignments`] trait pattern from the
/// bulk-update surface — same closure-return ergonomics, same sealed-
/// by-convention discipline.
pub trait IntoInsertColumns<S: Model, T: Model> {
    /// Flatten `self` into the ordered list of column mappings the
    /// INSERT...SELECT emitter renders as
    /// `INSERT (...) SELECT ... FROM ...`.
    fn into_insert_columns(self) -> Vec<InsertSelectColumn<S, T>>;
}

impl<S: Model, T: Model> IntoInsertColumns<S, T> for InsertSelectColumn<S, T> {
    fn into_insert_columns(self) -> Vec<InsertSelectColumn<S, T>> {
        vec![self]
    }
}

impl<S: Model, T: Model> IntoInsertColumns<S, T> for Vec<InsertSelectColumn<S, T>> {
    fn into_insert_columns(self) -> Vec<InsertSelectColumn<S, T>> {
        self
    }
}

/// Terminal-pending bulk `INSERT INTO target SELECT ... FROM source`.
/// [`InsertSelectStmt::execute`] emits the `INSERT ... SELECT` and
/// returns the affected row count.
/// The struct is `Clone` because [`QuerySet<S>`] is `Clone` and the
/// columns vector clones cheaply (each [`InsertSelectColumn<S, T>`]
/// carries a `&'static str` and a crate-private `ExprNode` tree that
/// is `Clone` already). Cloning an `InsertSelectStmt` to retry on a
/// transient failure (deadlock, serialization error) is a constant-
/// cost operation that does not re-run the user's column-mapping
/// closure.
/// `Clone` / `Debug` are hand-rolled (not derived) so they do not
/// require `S: Clone` / `T: Clone` / `S: Debug` / `T: Debug`
/// `InsertSelectStmt` never owns or borrows a `T`; the `PhantomData<fn() -> T>`
/// tag mirrors the variance pattern on [`QuerySet<S>`] and
/// [`crate::query::UpdateStmt`].
#[must_use = "InsertSelectStmt is inert — call .execute(ctx) to run the INSERT ... SELECT"]
pub struct InsertSelectStmt<S: Model, T: Model> {
    /// The source queryset — contributes the source `FROM` table name,
    /// the `WHERE` clause, ordering, limit, offset, and the
    /// `is_empty` short-circuit flag. The state-rejection checks in
    /// [`InsertSelectStmt::execute`] inspect this field.
    pub(crate) source: QuerySet<S>,
    /// The `(target_column, source_expression)` mappings built by the
    /// closure the user passed to [`QuerySet::insert_into`]. Ordered
    /// the emitter renders the INSERT column list and the SELECT
    /// projection in lockstep position.
    pub(crate) columns: Vec<InsertSelectColumn<S, T>>,
    /// Covariant `T` tag — matches [`QuerySet<T>`]'s variance so the
    /// statement composes with the same `Send + Sync` story.
    pub(crate) _target: PhantomData<fn() -> T>,
}

impl<S: Model, T: Model> Clone for InsertSelectStmt<S, T> {
    fn clone(&self) -> Self {
        InsertSelectStmt {
            source: self.source.clone(),
            columns: self.columns.clone(),
            _target: PhantomData,
        }
    }
}

impl<S: Model, T: Model> std::fmt::Debug for InsertSelectStmt<S, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InsertSelectStmt")
            .field("source_table", &S::table_name())
            .field("target_table", &T::table_name())
            .field("source", &self.source)
            .field("columns", &self.columns)
            .finish()
    }
}

impl<S: Model, T: Model> InsertSelectStmt<S, T> {
    /// Validate the column mapping and source-queryset state.
    /// Called by both [`execute`](InsertSelectStmt::execute) and
    /// [`execute_returning`](InsertSelectStmt::execute_returning) before
    /// they issue any SQL. Centralising the checks here means both
    /// terminals surface the same error classes (empty column list,
    /// duplicate target column, unsupported source-queryset state) even
    /// when the source queryset is [`QuerySet::none`]-derived — a
    /// silent `Ok` under `.none()` would otherwise mask mapping bugs
    /// until the `.none()` guard was removed.
    /// Returns `Ok(())` when validation passes.  Returns
    /// `Err(DjogiError::Validation(...))` for any of the rejection cases
    /// listed on [`execute`](InsertSelectStmt::execute).
    fn validate_execute(&self) -> Result<(), DjogiError> {
        // Validation: empty column list.
        if self.columns.is_empty() {
            return Err(DjogiError::Validation(format!(
                "insert_into::<{}>: column mapping is empty; an INSERT...SELECT \
                 with no columns is invalid SQL. The closure passed to \
                 QuerySet::insert_into must return at least one column mapping \
                 via FieldRef::copy_from",
                T::table_name(),
            )));
        }

        // Validation: duplicate target columns.
        let mut seen: HashSet<&'static str> = HashSet::with_capacity(self.columns.len());
        for col in &self.columns {
            if !seen.insert(col.target_column) {
                return Err(DjogiError::Validation(format!(
                    "insert_into::<{}>: target column '{}' appears more than \
                     once in the column mapping; Postgres rejects duplicate \
                     columns in an INSERT column list (SQLSTATE 42701)",
                    T::table_name(),
                    col.target_column,
                )));
            }
        }

        // Validation: reject unsupported source-queryset state.
        if !self.source.prefetch_paths.is_empty() {
            return Err(DjogiError::Validation(format!(
                "insert_into::<{}>: source queryset has registered prefetch \
                 paths, which have no meaning for INSERT...SELECT (no rows \
                 are returned to the caller). Drop the .prefetch(...) calls \
                 before .insert_into(...)",
                T::table_name(),
            )));
        }
        if !self.source.select_related_paths.is_empty() {
            return Err(DjogiError::Validation(format!(
                "insert_into::<{}>: source queryset has registered \
                 select_related paths, which expand the SELECT list with \
                 aliased joined columns the INSERT...SELECT column-mapping \
                 closure cannot reference. Drop the .select_related(...) \
                 calls before .insert_into(...)",
                T::table_name(),
            )));
        }
        if self.source.cache_target.is_some() {
            return Err(DjogiError::Validation(format!(
                "insert_into::<{}>: source queryset is bound to a Punnu via \
                 .cache(...). INSERT...SELECT returns the affected row count, \
                 not rows, so the cache binding has nothing to insert. Drop \
                 the .cache(...) call before .insert_into(...)",
                T::table_name(),
            )));
        }
        if !matches!(self.source.lock, crate::query::lock::LockMode::None) {
            return Err(DjogiError::Validation(format!(
                "insert_into::<{}>: source queryset carries a row-level lock \
                 (FOR UPDATE / FOR SHARE / NOWAIT / SKIP LOCKED) which is \
                 not yet supported on INSERT...SELECT in djogi v0.1. Drop \
                 the .select_for_update() / .nowait() / .skip_locked() / \
                 .select_for_share() / .for_share_nowait() / \
                 .for_share_skip_locked() call before .insert_into(...); a \
                 follow-up issue can lift this restriction with an \
                 explicit opt-in",
                T::table_name(),
            )));
        }
        if !matches!(
            self.source.distinct,
            crate::query::queryset::DistinctMode::None,
        ) {
            return Err(DjogiError::Validation(format!(
                "insert_into::<{}>: source queryset carries .distinct() / \
                 .distinct_on(...) which is not yet supported on \
                 INSERT...SELECT in djogi v0.1. Drop the .distinct...() call \
                 before .insert_into(...); a follow-up issue can lift this \
                 restriction with an explicit opt-in",
                T::table_name(),
            )));
        }

        Ok(())
    }

    /// Run the accumulated INSERT...SELECT and return the affected row
    /// count.
    /// # Validation rejections (no SQL issued, returns
    /// [`DjogiError::Validation`])
    /// Run **before** the `is_empty()` short-circuit so a programming
    /// error in the column mapping or source-queryset state still
    /// surfaces when the source happens to be
    /// [`QuerySet::none`]-derived. A silent `Ok(0)` under `.none()`
    /// would mask the bug until a caller removed the `.none()` (or it
    /// was guarded by an auth / feature-flag branch that flipped) — at
    /// which point the same SQL the framework would have rejected here
    /// would suddenly leak out as a live Postgres syntax error or
    /// SQLSTATE.
    /// - Empty column mapping (`columns.is_empty()`). Postgres would
    ///   reject `INSERT INTO t () SELECT ...` as syntactically invalid;
    ///   the framework pre-validates so the diagnostic carries the
    ///   target table name rather than the bare SQLSTATE.
    /// - Duplicate target column in the mapping (Postgres `42701`
    ///   surfaced before the SQL leaves the framework).
    /// - Source queryset carries `prefetch_paths`,
    ///   `select_related_paths`, a `cache_target`, a non-default
    ///   `LockMode`, or a non-default `DistinctMode`. See the module
    ///   docs for the rationale on each.
    /// # Short-circuit case (no SQL issued)
    /// After validation passes, returns `Ok(0)` without touching the
    /// database when the source queryset is [`QuerySet::none`]-derived
    /// (`is_empty() == true`).
    /// # Tenant / RLS auto-set
    /// Calls `auto_set_tenant` (the crate-private helper shared with
    /// every other write terminal) on the target model first, then on
    /// the source model. Both calls are idempotent (the helper checks
    /// `applied_tenant_id`), so the second call is a no-op when target
    /// and source share the same tenant key.
    /// # Return value
    /// Returns `u64` — the row count from `tokio_postgres`'s
    /// `CommandTag::rows_affected()`. Postgres' INSERT rowcount is
    /// non-negative by definition, so there is no sign conversion at
    /// the call site.
    pub fn execute<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<u64, DjogiError>> + Send + 'ctx
    where
        S: 'ctx,
        T: 'ctx,
    {
        async move {
            // Validation runs BEFORE the `is_empty` short-circuit so a
            // programming error in the column mapping or in the
            // source-queryset state still surfaces even when the source
            // queryset is `QuerySet::none()`-derived. A silent `Ok(0)`
            // would mask the bug until a caller removed the `.none()`
            // (or it was guarded by an auth / feature-flag branch that
            // flipped), at which point the same SQL the framework would
            // have rejected here would surface as a live Postgres
            // syntax error / SQLSTATE 42701 / etc. See
            // [`validate_execute`] for the full per-case rationale.
            self.validate_execute()?;

            // Short-circuit: structural-empty source. Runs AFTER the
            // validation block above so a `.none()` source with a
            // broken column mapping (empty list, duplicate target
            // column) or stale post-`.none()` state-adding chain still
            // surfaces the validation error rather than silently
            // succeeding. Mirrors the TASK6:empty_contract on bulk
            // update / delete in spirit, but applies validation first.
            if self.source.is_empty() {
                return Ok(0);
            }

            // Tenant / RLS auto-set. Target first (the INSERT target),
            // then source (the SELECT FROM). Both calls are idempotent
            // `auto_set_tenant` checks `applied_tenant_id` before
            // issuing the SET LOCAL — so the second call is a no-op
            // when target and source share the same tenant key, which
            // is the typical multi-tenant case.
            auto_set_tenant::<T>(ctx).await?;
            auto_set_tenant::<S>(ctx).await?;

            let acc = build_insert_select::<S, T>(&self.source, &self.columns)
                .map_err(DjogiError::from)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let rows_affected = ctx.execute(&sql, &params).await?;
            Ok(rows_affected)
        }
    }

    /// Run the accumulated INSERT...SELECT and return every inserted row
    /// as a decoded model instance.
    /// Uses PostgreSQL's canonical `RETURNING <column_list>` projection to
    /// retrieve the inserted rows in model order, including framework and user
    /// columns (`id`, `created_at`, `updated_at`) populated by target-table
    /// defaults. Rows are decoded via [`FromPgRow`].
    /// # Hooks and outbox
    /// Like [`execute`](InsertSelectStmt::execute), this terminal does
    /// **not** fire `before_save` / `after_save` hooks or enqueue outbox
    /// events. INSERT...SELECT is designed for bulk operations where
    /// per-row hooks would be prohibitively expensive. Adopters who need
    /// per-row side effects should follow up with a typed queryset over
    /// the returned IDs.
    /// # Warning — unbounded materialisation
    /// This method loads **one `T` per inserted row** into memory. For
    /// large copy operations, consider calling [`execute`] for the count
    /// and then querying the target table with a filter on the known ID
    /// range or a `created_at` window.
    /// # Validation rejections
    /// Same as [`execute`](InsertSelectStmt::execute): empty column
    /// mapping, duplicate target column, unsupported source-queryset
    /// state.
    /// # Short-circuit
    /// Returns `Ok(Vec::new())` when the source queryset is
    /// [`QuerySet::none`]-derived (`is_empty() == true`).
    pub fn execute_returning<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Vec<T>, DjogiError>> + Send + 'ctx
    where
        S: 'ctx,
        T: 'ctx + FromPgRow,
    {
        async move {
            // Same validation contract as execute().
            self.validate_execute()?;

            // Short-circuit for structural-empty source.
            if self.source.is_empty() {
                return Ok(Vec::new());
            }

            auto_set_tenant::<T>(ctx).await?;
            auto_set_tenant::<S>(ctx).await?;

            let acc = build_insert_select_returning::<S, T>(&self.source, &self.columns)
                .map_err(DjogiError::from)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let rows = ctx.query_all(&sql, &params).await?;
            let mut results = Vec::with_capacity(rows.len());
            for row in &rows {
                results.push(T::from_pg_row(row)?);
            }
            Ok(results)
        }
    }
}

impl<S: Model> QuerySet<S> {
    /// Build a bulk
    /// `INSERT INTO target_table (cols...) SELECT exprs... FROM source_table [WHERE ...] [ORDER BY ...] [LIMIT $n] [OFFSET $n]`
    /// statement that copies rows from this source queryset into the
    /// target model's table.
    /// The closure receives the target model's default-constructed
    /// `T::Fields` AND the source model's default-constructed
    /// `S::Fields`; it returns one or more typed
    /// [`InsertSelectColumn<S, T>`]s (either a single mapping or a
    /// `Vec`) via the [`FieldRef::copy_from`] builder. Each mapping
    /// pins the target column's value type to the source operand's
    /// value type at compile time, AND ties the source operand's model
    /// identity to the closure's source bag via the
    /// [`IntoInsertColumns<S, T>`] trait bound on the return type.
    /// # Framework-column semantics
    /// The target's framework columns (`id`, `created_at`,
    /// `updated_at`) are populated by their column-level `DEFAULT`
    /// clauses on the target table — the emitter never names them in
    /// the INSERT column list unless the caller's closure explicitly
    /// maps them. This matches [`Model::create`]'s contract: the
    /// caller's framework fields are ignored; the database populates
    /// them.
    /// # Returned type
    /// Returns an inert [`InsertSelectStmt<S, T>`] — the actual SQL
    /// runs when the caller invokes [`InsertSelectStmt::execute`] with
    /// a `&mut DjogiContext`. Splitting the builder from the terminal
    /// matches the [`QuerySet::update`] shape; callers can log,
    /// inspect, or retry the pending statement without re-running the
    /// closure.
    /// # Rejected source state
    /// The terminal returns [`DjogiError::Validation`] when the source
    /// queryset carries state that cannot be safely represented in an
    /// INSERT...SELECT shape: `prefetch`, `select_related`, `cache`,
    /// a non-default `LockMode`, or a non-default `DistinctMode`. See
    /// the module docs for the rationale on each.
    /// # Example — archival pattern
    /// ```ignore
    /// use djogi::prelude::*;
    ///
    /// // Archive completed orders older than the cutoff into an
    /// // archive table. The archive carries a separate id column
    /// // (`original_id`) so the source's id is preserved as a user
    /// // column without colliding with the target's framework `id`.
    /// //
    /// // `_, _` are the closure type and the return-shape type — Rust
    /// // infers both. The target model `T` is the one type parameter
    /// // the call site must name because neither argument carries it.
    /// CompletedOrder::objects()
    ///     .filter(|f| f.completed_at().lt(cutoff))
    ///     .insert_into::<OrderArchive, _, _>(|target, source| vec![
    ///         target.original_id().copy_from(source.id().as_insert_source()),
    ///         target.title().copy_from(source.title().as_insert_source()),
    ///         target.completed_at().copy_from(source.completed_at().as_insert_source()),
    ///     ])
    ///     .execute(&mut ctx)
    ///     .await?;
    /// ```
    /// # See also
    /// - — the originating gap analysis.
    /// - [`QuerySet::update`] — the sibling bulk-write terminal for
    ///   in-place row mutation.
    /// - [`Model::create`] / `Model::bulk_create` — the row-by-row
    ///   and small-batch INSERT paths.
    #[must_use = "InsertSelectStmt is inert — call .execute(ctx) to run the INSERT ... SELECT"]
    pub fn insert_into<T, F, I>(self, f: F) -> InsertSelectStmt<S, T>
    where
        T: Model,
        F: FnOnce(T::Fields, S::Fields) -> I,
        I: IntoInsertColumns<S, T>,
    {
        let columns = f(T::Fields::default(), S::Fields::default()).into_insert_columns();
        InsertSelectStmt {
            source: self,
            columns,
            _target: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the builder surface — no SQL, no executor. Live
    //! DB coverage is in `tests/integration/insert_select.rs`.
    //! We reach through the `FieldRef` API to build column mappings so
    //! the `pub(crate)` fields on [`InsertSelectColumn`] never leak
    //! into the test module's observed surface (same pattern as the
    //! Task 8 `FilterClause` tests and the bulk-update builder tests).

    use super::*;
    use crate::descriptor::ModelDescriptor;
    use crate::query::field::FieldRef;

    // Minimal `Model` impls — mirror the `Fake` used in `query::field`,
    // `query::sql`, and `query::update` unit tests so this file's
    // checks stay independent of `#[model]` macro expansion.
    struct Source;
    impl crate::model::__sealed::Sealed for Source {}
    #[allow(clippy::manual_async_fn)]
    impl Model for Source {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "sources"
        }
        fn pk_value(&self) -> &i64 {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            unreachable!()
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: i64,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
    }

    struct Target;
    impl crate::model::__sealed::Sealed for Target {}
    #[allow(clippy::manual_async_fn)]
    impl Model for Target {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "targets"
        }
        fn pk_value(&self) -> &i64 {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            unreachable!()
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: i64,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
    }

    #[test]
    fn field_ref_copy_from_field_builds_column_mapping() {
        let target_col: FieldRef<Target, i32> = FieldRef::new("view_count");
        let source_col: FieldRef<Source, i32> = FieldRef::new("score");
        let mapping: InsertSelectColumn<Source, Target> =
            target_col.copy_from(source_col.as_insert_source());
        assert_eq!(mapping.target_column(), "view_count");
        // Source is a bare field reference — ExprNode::Field.
        assert!(matches!(
            mapping.source(),
            ExprNode::Field { column } if *column == "score"
        ));
    }

    #[test]
    fn field_ref_copy_from_literal_builds_column_mapping() {
        let target_col: FieldRef<Target, i32> = FieldRef::new("status_code");
        // `InsertSelectSource::literal` is polymorphic in `S`; the
        // explicit annotation here pins `S = Source` so the test type-
        // checks without relying on closure inference. Real adopter code
        // gets `S` inferred from the closure return type.
        let mapping: InsertSelectColumn<Source, Target> =
            target_col.copy_from(InsertSelectSource::<Source, _>::literal(42i32));
        assert_eq!(mapping.target_column(), "status_code");
        // Source is a literal — ExprNode::Literal.
        assert!(matches!(mapping.source(), ExprNode::Literal(_)));
    }

    #[test]
    fn into_insert_columns_single_wraps_in_vec() {
        let target_col: FieldRef<Target, i32> = FieldRef::new("view_count");
        let source_col: FieldRef<Source, i32> = FieldRef::new("score");
        let mapping = target_col.copy_from(source_col.as_insert_source());
        let v: Vec<InsertSelectColumn<Source, Target>> = mapping.into_insert_columns();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].target_column(), "view_count");
    }

    #[test]
    fn into_insert_columns_vec_passes_through() {
        let target_a: FieldRef<Target, i32> = FieldRef::new("view_count");
        let target_b: FieldRef<Target, bool> = FieldRef::new("published");
        let source_a: FieldRef<Source, i32> = FieldRef::new("score");
        let source_b: FieldRef<Source, bool> = FieldRef::new("active");
        let vs: Vec<InsertSelectColumn<Source, Target>> = vec![
            target_a.copy_from(source_a.as_insert_source()),
            target_b.copy_from(source_b.as_insert_source()),
        ];
        let out = vs.into_insert_columns();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].target_column(), "view_count");
        assert_eq!(out[1].target_column(), "published");
    }

    #[test]
    fn insert_into_builds_stmt_with_columns() {
        let qs: QuerySet<Source> = QuerySet::new();
        let stmt = qs.insert_into::<Target, _, _>(|_t, _s| {
            // Build a small column mapping; closure exercises the
            // two-argument shape (target_fields, source_fields).
            let target_col: FieldRef<Target, i32> = FieldRef::new("view_count");
            let source_col: FieldRef<Source, i32> = FieldRef::new("score");
            vec![target_col.copy_from(source_col.as_insert_source())]
        });
        assert_eq!(stmt.columns.len(), 1);
        assert_eq!(stmt.columns[0].target_column(), "view_count");
    }

    #[test]
    fn insert_select_stmt_clones_preserve_columns() {
        // `InsertSelectStmt: Clone` is documented on the struct
        // assert the clone preserves the columns list without
        // re-running the user's closure (there isn't one to re-run
        // post-build).
        let qs: QuerySet<Source> = QuerySet::new();
        let stmt = qs.insert_into::<Target, _, _>(|_t, _s| {
            let target_col: FieldRef<Target, i32> = FieldRef::new("view_count");
            let source_col: FieldRef<Source, i32> = FieldRef::new("score");
            vec![target_col.copy_from(source_col.as_insert_source())]
        });
        let cloned = stmt.clone();
        assert_eq!(cloned.columns.len(), 1);
        assert_eq!(cloned.columns[0].target_column(), "view_count");
    }

    #[test]
    fn insert_select_stmt_single_mapping_via_into_insert_columns() {
        // Closure returns a single InsertSelectColumn rather than a
        // Vec — IntoInsertColumns wraps it in a 1-element Vec.
        let qs: QuerySet<Source> = QuerySet::new();
        let stmt = qs.insert_into::<Target, _, _>(|_t, _s| {
            let target_col: FieldRef<Target, i32> = FieldRef::new("view_count");
            let source_col: FieldRef<Source, i32> = FieldRef::new("score");
            target_col.copy_from(source_col.as_insert_source())
        });
        assert_eq!(stmt.columns.len(), 1);
        assert_eq!(stmt.columns[0].target_column(), "view_count");
    }

    #[test]
    fn insert_select_source_arithmetic_composes_same_source_tag() {
        // `source.col + literal` — Numeric arithmetic on
        // `InsertSelectSource<S, V>` produces an `InsertSelectSource<S, V>`
        // whose source tag matches the operand's. The compile-fail
        // sibling for cross-source arithmetic lives at
        // `djogi/tests/compile_fail/insert_select_cross_source_arithmetic.rs`.
        let source_col: FieldRef<Source, i32> = FieldRef::new("score");
        let target_col: FieldRef<Target, i32> = FieldRef::new("view_count");
        let composed: InsertSelectSource<Source, i32> =
            source_col.as_insert_source() + InsertSelectSource::<Source, _>::literal(1i32);
        let mapping: InsertSelectColumn<Source, Target> = target_col.copy_from(composed);
        assert_eq!(mapping.target_column(), "view_count");
        // The composed node is an Add — leaf is bare-Field + Literal.
        assert!(matches!(mapping.source(), ExprNode::Add(_, _)));
    }
}
