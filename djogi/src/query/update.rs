//! Bulk update — `QuerySet::update(|f| f.col.set(v))` + `QuerySet::delete`.
//!
//! # What
//!
//! [`UpdateAssignment`] is a single `SET column = value` leaf produced by
//! [`FieldRef::set`]. [`IntoAssignments`] is the closure-return shape — one
//! assignment or many — that [`QuerySet::update`] accepts. [`UpdateStmt`]
//! is the terminal-pending struct the builder returns; the actual `UPDATE`
//! runs when the caller invokes [`UpdateStmt::execute`] with an executor.
//!
//! The sibling `.delete(...)` terminal lives on [`QuerySet`] directly rather
//! than going through an `UpdateStmt`-style intermediate — DELETE has no
//! payload to carry across a builder/terminal split, so it is wired the
//! same way [`QuerySet::fetch_all`] and friends are.
//!
//! # Why the terminal split for UPDATE
//!
//! `.update(|f| ...)` returns a pending [`UpdateStmt`] rather than a future
//! because the builder shape is symmetric with the read terminals
//! ([`QuerySet::fetch_all`] also takes the executor *at terminal time*,
//! not during builder accumulation). The intermediate type also lets
//! callers:
//!
//! - **Log or inspect** the queued assignments before execution.
//! - **Retry** an UPDATE without re-running the filter closure. `UpdateStmt`
//!   is `Clone` because [`QuerySet`] is `Clone`; cloning the statement is
//!   exactly as cheap as cloning the underlying queryset.
//! - **Branch on short-circuit**: callers that want to know "did I skip the
//!   DB round-trip?" can inspect `qs.is_empty()` / `assignments.is_empty()`
//!   themselves before calling `.execute(...)`. The default path still
//!   short-circuits on both conditions inside [`UpdateStmt::execute`].
//!
//! # Constructor-only invariant on `UpdateAssignment`
//!
//! `UpdateAssignment`'s fields are `pub(crate)`; the only way to build one
//! from outside this crate is [`FieldRef::set`], which funnels through
//! [`IntoFilterValue`]. This mirrors [`crate::query::filter::FilterClause`]'s
//! lock-down added in Task 8: there is no "build a raw assignment" escape
//! hatch, so the column literal is always macro-baked and the value is
//! always a structurally-valid scalar `FilterValue` (never a `List`, `Pair`,
//! or `Null`, which the UPDATE emitter has no sensible rendering for).
//!
//! Users who need `col = col + N`, `col = other_col`, or other
//! field-vs-field / arithmetic assignments reach for the expression
//! builder [`FieldRef::set_expr`]: it wraps an [`crate::expr::Expr<V>`]
//! IR tree into the same [`UpdateAssignment`] shape the literal `.set(v)`
//! produces. For richer SQL the emitter cannot express (`NOW() - interval
//! '1 day'`, `CASE WHEN ...` before Task 5 lands), the raw
//! `SqlAccumulator` escape hatch in [`crate::raw`] is still there.
//!
//! # `updated_at = now()` stamping
//!
//! The SQL emitter ([`build_update`]) always appends `updated_at = now()`
//! to the SET list, even when the caller's closure omits it. Parity with
//! the single-row [`crate::model::Model::save`] path, which also bumps
//! `updated_at` on every write. Users who need to preserve `updated_at`
//! across a bulk update reach for the raw escape hatch — same as any
//! other ORM layer that makes auditing hard to bypass.
//!
//! # `is_empty` short-circuit
//!
//! Both terminals honour the `TASK6:empty_contract`: a queryset derived
//! from [`QuerySet::none`] (or an empty assignment list, for `update`)
//! returns `Ok(0)` without issuing any SQL. The grep marker lives on the
//! `is_empty` field in `queryset.rs` — if that field's shape ever
//! changes, every terminal that honours it surfaces through the marker.
//!
//! [`build_update`]: crate::query::sql::build_update
//! [`FieldRef`]: crate::query::field::FieldRef
//! [`QuerySet`]: crate::query::queryset::QuerySet
#![allow(clippy::manual_async_fn)]

use crate::DjogiError;
use crate::context::DjogiContext;
use crate::model::Model;
use crate::pg::accumulator::as_params;
use crate::query::condition::FilterValue;
use crate::query::field::{FieldRef, IntoFilterValue};
use crate::query::queryset::QuerySet;
use crate::query::sql::{build_delete, build_update};
use crate::query::terminal::auto_set_tenant;
use std::future::Future;
use std::marker::PhantomData;

/// A single `SET column = value` clause — produced by [`FieldRef::set`]
/// (literal) or [`FieldRef::set_expr`] (expression IR).
///
/// # Invariants
///
/// Fields are `pub(crate)` so the only way to construct an
/// `UpdateAssignment` from outside this crate is [`FieldRef::set`] or
/// [`FieldRef::set_expr`]. The literal path funnels through
/// [`IntoFilterValue`], so a `Literal` payload is always a structurally-
/// valid scalar `FilterValue` (never `List`, `Pair`, or `Null`). The
/// expression path takes a typed `Expr<V>` wrapper; the inner
/// `ExprNode` is crate-private so downstream code cannot fabricate
/// unsafe tree shapes. `column` is always a macro-baked `&'static str`.
///
/// `Debug` + `Clone` are derived so callers can log pending assignments
/// and retry an `UpdateStmt` without re-running the builder closure.
#[derive(Debug, Clone)]
pub struct UpdateAssignment {
    /// SQL column name — macro-baked literal, never user input.
    pub(crate) column: &'static str,
    /// Assignment payload — either a projected literal or an IR tree.
    pub(crate) value: AssignmentValue,
}

/// The right-hand side of an `UpdateAssignment`.
///
/// `Literal` carries a single bind value (the Phase 2 shape). `Expr`
/// carries an IR node whose emission (`col = col + 1`, `col = other_col`,
/// arithmetic combinations) is handled by the Phase 4 expression emitter.
#[derive(Debug, Clone)]
pub(crate) enum AssignmentValue {
    /// Literal value — emitted as a single `$n` bind via
    /// [`crate::query::sql::push_filter_value`].
    Literal(FilterValue),
    /// Expression IR — emitted via
    /// [`crate::expr::sql::emit_expr`]; the emitter recurses through
    /// arithmetic, field refs, and comparisons.
    Expr(crate::expr::node::ExprNode),
}

impl UpdateAssignment {
    /// Internal constructor for literal assignments — called only by
    /// [`FieldRef::set`]. Kept `pub(crate)` because the public-API path
    /// is the typed `FieldRef::set` surface; hand-building an assignment
    /// from downstream code would bypass the `V: IntoFilterValue` type
    /// check.
    pub(crate) fn new(column: &'static str, value: FilterValue) -> Self {
        Self {
            column,
            value: AssignmentValue::Literal(value),
        }
    }

    /// Internal constructor for expression-backed assignments — called
    /// only by [`FieldRef::set_expr`]. The `ExprNode` tree is already
    /// `V`-safe at construction; the emitter walks it directly.
    pub(crate) fn new_expr(column: &'static str, node: crate::expr::node::ExprNode) -> Self {
        Self {
            column,
            value: AssignmentValue::Expr(node),
        }
    }

    /// Internal accessor for the column name. Used by the SQL emitter
    /// ([`crate::query::sql::build_update`]) to render the SET clause.
    #[doc(hidden)]
    pub fn column(&self) -> &'static str {
        self.column
    }

    /// Internal accessor for the assignment payload. Used by the SQL
    /// emitter to dispatch between the literal-bind and expression-IR
    /// emission paths.
    #[doc(hidden)]
    pub(crate) fn value(&self) -> &AssignmentValue {
        &self.value
    }
}

/// Typed constructor — `field.set(value)` produces a single
/// [`UpdateAssignment`] that slots into the closure passed to
/// [`QuerySet::update`].
///
/// Mirrors the `V: IntoFilterValue` bound every other [`FieldRef`] lookup
/// method uses, so newtype columns and string-like types compose the same
/// way they do in `.eq` / `.gte` / `.in_list`.
///
/// ```ignore
/// Post::objects()
///     .filter(|f| f.published().eq(true))
///     .update(|f| f.view_count().set(999i32))
///     .execute(&mut ctx).await?;
/// ```
impl<M: Model, V: IntoFilterValue> FieldRef<M, V> {
    /// Build a typed `SET column = value` assignment for
    /// [`QuerySet::update`].
    #[must_use = "assignments are lazy — drop one and the SET clause is silently omitted"]
    pub fn set(self, value: V) -> UpdateAssignment {
        UpdateAssignment::new(self.column(), value.into_filter_value())
    }
}

/// Expression-backed assignment builder — `field.set_expr(Expr<V>)`
/// produces an [`UpdateAssignment`] whose SQL right-hand side is the
/// compiled IR tree rather than a single bind.
///
/// Typical call sites:
///
/// ```ignore
/// // col = col + 1
/// Account::objects()
///     .update(|f| f.balance().set_expr(f.balance().as_expr() + Expr::literal(1i64)))
///     .execute(&mut ctx).await?;
///
/// // col = other_col  (field-vs-field copy)
/// Account::objects()
///     .update(|f| f.balance().set_expr(f.overdraft_limit().as_expr()))
///     .execute(&mut ctx).await?;
/// ```
///
/// The `V: IntoFilterValue` bound is kept for symmetry with
/// [`FieldRef::set`] — both entry points flow through the same typed
/// surface, and the `ExprNode` tree has already committed to a value
/// type by the time the user reaches this method.
impl<M: Model, V: IntoFilterValue> FieldRef<M, V> {
    /// Build an expression-backed `SET column = <expr>` assignment for
    /// [`QuerySet::update`]. `<expr>` composes field references,
    /// arithmetic, and literals via the [`crate::expr::Expr<V>`] IR.
    #[must_use = "assignments are lazy — drop one and the SET clause is silently omitted"]
    pub fn set_expr(self, expr: crate::expr::Expr<V>) -> UpdateAssignment {
        UpdateAssignment::new_expr(self.column(), expr.node)
    }
}

/// Closure-return shape for [`QuerySet::update`]. The closure can return
/// a single [`UpdateAssignment`] or a `Vec<UpdateAssignment>` — this trait
/// bridges both so the user writes the natural thing at the call site.
///
/// Sealed-by-convention: only the two shipped impls (`UpdateAssignment`
/// and `Vec<UpdateAssignment>`) exist, and there is no public trait method
/// that a downstream impl would add value beyond. Users do not implement
/// this trait by hand.
pub trait IntoAssignments {
    /// Flatten `self` into the ordered list of assignments the UPDATE
    /// emitter renders as `SET col = $n, col = $n, ...`.
    fn into_assignments(self) -> Vec<UpdateAssignment>;
}

impl IntoAssignments for UpdateAssignment {
    fn into_assignments(self) -> Vec<UpdateAssignment> {
        vec![self]
    }
}

impl IntoAssignments for Vec<UpdateAssignment> {
    fn into_assignments(self) -> Vec<UpdateAssignment> {
        self
    }
}

/// Terminal-pending bulk update. [`UpdateStmt::execute`] emits the
/// `UPDATE` and returns the affected row count.
///
/// The struct is `Clone` because [`QuerySet`] is `Clone` — both fields
/// are cheap structural vectors, so cloning an `UpdateStmt` to retry a
/// transient failure (deadlock, serialization error) is a constant-time
/// operation that does not re-run the user's builder closure.
///
/// `Clone` / `Debug` are hand-rolled (not derived) so they do not
/// require `T: Clone` / `T: Debug` — `UpdateStmt` never owns or borrows
/// a `T`, it only carries a `PhantomData<fn() -> T>` tag, mirroring the
/// pattern on [`QuerySet<T>`].
#[must_use = "UpdateStmt is inert — call .execute(ctx) to run the UPDATE"]
pub struct UpdateStmt<T: Model> {
    /// The accumulated queryset — contributes the `WHERE` clause and the
    /// `is_empty` short-circuit flag.
    pub(crate) qs: QuerySet<T>,
    /// The `SET col = $n, ...` payload built by the closure the user
    /// passed to [`QuerySet::update`].
    pub(crate) assignments: Vec<UpdateAssignment>,
    /// Covariant `T` tag — matches [`QuerySet<T>`]'s variance so an
    /// `UpdateStmt<T>` composes with the same `Send + Sync` story.
    pub(crate) _m: PhantomData<fn() -> T>,
}

impl<T: Model> Clone for UpdateStmt<T> {
    fn clone(&self) -> Self {
        UpdateStmt {
            qs: self.qs.clone(),
            assignments: self.assignments.clone(),
            _m: PhantomData,
        }
    }
}

impl<T: Model> std::fmt::Debug for UpdateStmt<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateStmt")
            .field("table", &T::table_name())
            .field("qs", &self.qs)
            .field("assignments", &self.assignments)
            .finish()
    }
}

impl<T: Model> UpdateStmt<T> {
    /// Run the accumulated UPDATE and return the affected row count.
    ///
    /// Short-circuits to `Ok(0)` when either:
    /// - The underlying queryset is `QuerySet::none()`-derived
    ///   (`is_empty()` is `true`), or
    /// - The closure produced zero assignments — `UPDATE ... SET ...`
    ///   with an empty SET list is a Postgres syntax error, so the
    ///   short-circuit here is both a contract shortcut and a safety
    ///   rail.
    ///
    /// Takes `&mut DjogiContext`, matching [`QuerySet::fetch_all`] /
    /// [`QuerySet::count`] — the same call site works against a pool-
    /// backed context or a transaction-backed one post-Phase-4 retrofit.
    ///
    /// Returns `u64` — the row-count from `tokio_postgres`'s
    /// `CommandTag::rows_affected()`. Postgres' UPDATE rowcount is
    /// non-negative by definition, so there is no sign conversion at
    /// the call site.
    pub fn execute<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<u64, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
    {
        async move {
            // TASK6:empty_contract — structural-none queryset OR empty
            // assignment list: return `Ok(0)` without touching the DB.
            //
            // The assignment-list branch is load-bearing: a closure that
            // produces `vec![]` would otherwise lead to
            // `UPDATE <table> SET , updated_at = now() WHERE ...`, which
            // Postgres rejects with a syntax error. Short-circuiting here
            // keeps the user's call site free of "why did this panic?"
            // and matches the structural-empty contract on `QuerySet::none()`.
            if self.qs.is_empty() || self.assignments.is_empty() {
                return Ok(0);
            }
            auto_set_tenant::<T>(ctx).await?;
            let acc = build_update(&self.qs, &self.assignments).map_err(crate::DjogiError::from)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let rows_affected = ctx.execute(&sql, &params).await?;
            // TODO(8δ T7.5 follow-up): bulk-update cache invalidation.
            // The safe path (Option B — opt-in `with_cache_invalidation()`
            // builder) requires adding an `invalidate: bool` flag to
            // `UpdateStmt`, switching the SQL to `UPDATE ... RETURNING id`
            // when set, and enqueuing one `on_commit` callback per touched
            // row id. Deferred because the current `execute` path uses
            // `ctx.execute` (row-count only), and changing to `RETURNING id`
            // changes the SQL shape in a way that needs design review to
            // ensure no existing call site breaks. The single-row
            // `Model::save` and `Model::delete` paths are the primary cache
            // invalidation surface; bulk-update invalidation is a follow-up.
            // The no-op test placeholder (bulk_update_invalidation_deferred_to_followup)
            // in phase8_t7_5_cache_invalidation.rs has been removed; bulk-update
            // invalidation will be tested when Option B is implemented.
            Ok(rows_affected)
        }
    }
}

impl<T: Model> QuerySet<T> {
    /// Build a bulk `UPDATE <table> SET col = val, ... [WHERE ...]`
    /// statement. The closure receives the model's default-constructed
    /// `Fields` handle and returns one or more typed
    /// [`UpdateAssignment`]s (either a single assignment or a `Vec`).
    ///
    /// Two assignment forms are accepted in the closure:
    ///
    /// - Literal: `f.col().set(value)` where `value: V: IntoFilterValue`.
    /// - Expression IR: `f.col().set_expr(expr)` for `col = col + 1`,
    ///   `col = NOW()`, `col = other_col`, and similar shapes the
    ///   [`crate::expr`] builder supports.
    ///
    /// For SQL the expression builder cannot express, reach for
    /// [`DjogiContext::raw_execute`](crate::DjogiContext::raw_execute).
    ///
    /// The returned [`UpdateStmt`] is inert — the actual SQL runs when
    /// the caller invokes [`UpdateStmt::execute`] with a
    /// `&mut DjogiContext`. Splitting the builder from the terminal keeps
    /// the call-site shape symmetric with the read terminals
    /// (`fetch_all`, `count`, etc.) and lets callers log, inspect, or
    /// retry the pending statement without re-running the closure.
    ///
    /// ```ignore
    /// Post::objects()
    ///     .filter(|f| f.published().eq(true))
    ///     .update(|f| f.view_count().set(999i32))
    ///     .execute(&mut ctx)
    ///     .await?;
    /// ```
    #[must_use = "UpdateStmt is inert — call .execute(ctx) to run the UPDATE"]
    pub fn update<F, A>(self, f: F) -> UpdateStmt<T>
    where
        F: FnOnce(T::Fields) -> A,
        A: IntoAssignments,
    {
        let assignments = f(T::Fields::default()).into_assignments();
        UpdateStmt {
            qs: self,
            assignments,
            _m: PhantomData,
        }
    }

    /// Run `DELETE FROM <table> [WHERE ...]` and return the affected row
    /// count.
    ///
    /// Unlike [`QuerySet::update`], DELETE carries no payload across a
    /// builder/terminal split, so this method is a terminal directly —
    /// same shape as [`QuerySet::fetch_all`] / [`QuerySet::count`].
    ///
    /// Short-circuits to `Ok(0)` for `QuerySet::none()`-derived
    /// querysets (the `TASK6:empty_contract`). A DELETE with no WHERE
    /// clause (an unfiltered queryset) is still a real DELETE — it
    /// removes every row in the table. Callers who want "wipe this
    /// table" DDL-style reach for `TRUNCATE` via
    /// [`DjogiContext::raw_execute`](crate::DjogiContext::raw_execute);
    /// this method only runs `DELETE FROM`.
    ///
    /// ```ignore
    /// Post::objects()
    ///     .filter(|f| f.published().eq(false))
    ///     .delete(&mut ctx)
    ///     .await?;
    /// ```
    pub fn delete<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<u64, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
    {
        async move {
            // TASK6:empty_contract — structural-none queryset: no SQL.
            if self.is_empty() {
                return Ok(0);
            }
            auto_set_tenant::<T>(ctx).await?;
            let acc = build_delete(&self).map_err(crate::DjogiError::from)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let rows_affected = ctx.execute(&sql, &params).await?;
            Ok(rows_affected)
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the builder surface — no SQL, no executor. Live
    //! DB coverage is in `tests/integration/phase2_queryset.rs`.
    //!
    //! We reach through the `FieldRef` API to build assignments so the
    //! `pub(crate)` fields on `UpdateAssignment` never leak into the
    //! test module's observed surface (same pattern as the Task 8
    //! `FilterClause` tests).

    use super::*;
    use crate::descriptor::ModelDescriptor;
    use crate::query::field::FieldRef;

    // Minimal `Model` impl — mirrors the `Fake` used in `query::field` and
    // `query::sql` unit tests so this file's checks stay independent of
    // `#[model]` macro expansion.
    struct Fake;
    impl crate::model::__sealed::Sealed for Fake {}
    #[allow(clippy::manual_async_fn)]
    impl Model for Fake {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "fakes"
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
    fn field_ref_set_builds_assignment_with_projected_value() {
        let f: FieldRef<Fake, i32> = FieldRef::new("view_count");
        let a = f.set(42i32);
        assert_eq!(a.column(), "view_count");
        assert!(matches!(
            a.value(),
            crate::query::update::AssignmentValue::Literal(FilterValue::I32(42))
        ));
    }

    #[test]
    fn field_ref_set_expr_builds_expression_assignment() {
        use crate::expr::Expr;
        let f: FieldRef<Fake, i64> = FieldRef::new("balance");
        // balance + 5
        let a = f.set_expr(f.as_expr() + Expr::literal(5i64));
        assert_eq!(a.column(), "balance");
        assert!(matches!(
            a.value(),
            crate::query::update::AssignmentValue::Expr(_)
        ));
    }

    #[test]
    fn into_assignments_single_wraps_in_vec() {
        let f: FieldRef<Fake, bool> = FieldRef::new("published");
        let a = f.set(true);
        let v = a.into_assignments();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].column(), "published");
    }

    #[test]
    fn into_assignments_vec_passes_through() {
        let a: FieldRef<Fake, i32> = FieldRef::new("view_count");
        let b: FieldRef<Fake, bool> = FieldRef::new("published");
        let vs = vec![a.set(0i32), b.set(false)];
        let out = vs.into_assignments();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].column(), "view_count");
        assert_eq!(out[1].column(), "published");
    }

    #[test]
    fn update_stmt_clones_preserve_assignments() {
        // `UpdateStmt: Clone` is documented on the struct — assert the
        // clone preserves the assignment list without re-running any
        // closure (there isn't one to re-run post-build).
        let f: FieldRef<Fake, i32> = FieldRef::new("view_count");
        let qs: QuerySet<Fake> = QuerySet::new();
        let stmt = qs.update(|_| f.set(42i32));
        let cloned = stmt.clone();
        assert_eq!(cloned.assignments.len(), 1);
        assert_eq!(cloned.assignments[0].column(), "view_count");
    }
}
