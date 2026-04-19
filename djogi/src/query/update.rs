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
//! Users who need richer SQL (`col = col + 1`, `col = NOW() - interval`,
//! `col = other_col`) use the raw `sqlx::QueryBuilder` escape hatch in
//! [`crate::raw`]; expression-backed SET lands in Phase 4 alongside the
//! rest of the expression layer.
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
use crate::model::Model;
use crate::query::condition::FilterValue;
use crate::query::field::{FieldRef, IntoFilterValue};
use crate::query::queryset::QuerySet;
use crate::query::sql::{build_delete, build_update};
use std::future::Future;
use std::marker::PhantomData;

/// A single `SET column = value` clause — produced by [`FieldRef::set`].
///
/// # Invariants
///
/// Fields are `pub(crate)` so the only way to construct an
/// `UpdateAssignment` from outside this crate is [`FieldRef::set`]. That
/// funnel routes the value through [`IntoFilterValue`], so `value` is
/// always a structurally-valid scalar `FilterValue` (never `List`,
/// `Pair`, or `Null`) and `column` is always a macro-baked
/// `&'static str`. The UPDATE emitter ([`crate::query::sql::build_update`])
/// relies on both invariants — the same reasoning that makes `FilterClause`'s
/// fields `pub(crate)` in Task 8.
///
/// `Debug` + `Clone` are derived so callers can log pending assignments
/// and retry an `UpdateStmt` without re-running the builder closure.
#[derive(Debug, Clone)]
pub struct UpdateAssignment {
    /// SQL column name — macro-baked literal, never user input.
    pub(crate) column: &'static str,
    /// Already-projected bind value. Scalar `FilterValue` only; the
    /// UPDATE emitter routes this through `push_filter_value`, which
    /// panics on `List`/`Pair`/`Null`. The `FieldRef::set` constructor
    /// and `IntoFilterValue` together guarantee we never reach that
    /// panic.
    pub(crate) value: FilterValue,
}

impl UpdateAssignment {
    /// Internal constructor — called only by [`FieldRef::set`]. Kept
    /// `pub(crate)` because the public-API path is the typed
    /// `FieldRef::set` surface; hand-building an assignment from
    /// downstream code would bypass the `V: IntoFilterValue` type check.
    pub(crate) fn new(column: &'static str, value: FilterValue) -> Self {
        Self { column, value }
    }

    /// Internal accessor for the column name. Used by the SQL emitter
    /// ([`crate::query::sql::build_update`]) to render the SET clause.
    #[doc(hidden)]
    pub fn column(&self) -> &'static str {
        self.column
    }

    /// Internal accessor for the bound value. Used by the SQL emitter
    /// when pushing the `SET col = $n` bind.
    #[doc(hidden)]
    pub fn value(&self) -> &FilterValue {
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
///     .execute(&pool).await?;
/// ```
impl<M: Model, V: IntoFilterValue> FieldRef<M, V> {
    /// Build a typed `SET column = value` assignment for
    /// [`QuerySet::update`].
    #[must_use = "assignments are lazy — drop one and the SET clause is silently omitted"]
    pub fn set(self, value: V) -> UpdateAssignment {
        UpdateAssignment::new(self.column(), value.into_filter_value())
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
#[must_use = "UpdateStmt is inert — call .execute(executor) to run the UPDATE"]
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
    /// The executor generic mirrors [`QuerySet::fetch_all`] /
    /// [`QuerySet::count`] — `&PgPool` and `&mut *tx` both satisfy the
    /// bound, so callers can run the UPDATE against the pool directly or
    /// inside a Phase 4 transaction without changing the call site.
    ///
    /// Returns `u64` — the raw row-count sqlx surfaces from
    /// [`sqlx::postgres::PgQueryResult::rows_affected`]. Postgres' UPDATE
    /// rowcount is non-negative by definition, so there is no sign
    /// conversion at the call site.
    pub fn execute<'a, E>(self, executor: E) -> impl Future<Output = Result<u64, DjogiError>> + Send
    where
        E: sqlx::Executor<'a, Database = sqlx::Postgres> + Send,
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
            let mut qb = build_update(&self.qs, &self.assignments);
            let result = qb
                .build()
                .execute(executor)
                .await
                .map_err(DjogiError::from)?;
            Ok(result.rows_affected())
        }
    }
}

impl<T: Model> QuerySet<T> {
    /// Build a bulk `UPDATE <table> SET col = val, ... [WHERE ...]`
    /// statement. The closure receives the model's default-constructed
    /// `Fields` handle and returns one or more typed
    /// [`UpdateAssignment`]s (either a single assignment or a `Vec`).
    ///
    /// Phase 2 supports literal assignments only — `f.col().set(value)`
    /// where `value: V: IntoFilterValue`. Expression-backed SET
    /// (`col = col + 1`, `col = NOW()`, `col = other_col`) lands in
    /// Phase 4 alongside the rest of the expression layer; until then,
    /// `djogi::raw::execute` is the documented escape hatch.
    ///
    /// The returned [`UpdateStmt`] is inert — the actual SQL runs when
    /// the caller invokes [`UpdateStmt::execute`] with a
    /// `sqlx::Executor`. Splitting the builder from the terminal keeps
    /// the call-site shape symmetric with the read terminals
    /// (`fetch_all`, `count`, etc.) and lets callers log, inspect, or
    /// retry the pending statement without re-running the closure.
    ///
    /// ```ignore
    /// Post::objects()
    ///     .filter(|f| f.published().eq(true))
    ///     .update(|f| f.view_count().set(999i32))
    ///     .execute(&pool)
    ///     .await?;
    /// ```
    #[must_use = "UpdateStmt is inert — call .execute(executor) to run the UPDATE"]
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
    /// [`crate::raw::execute`]; this method only runs `DELETE FROM`.
    ///
    /// ```ignore
    /// Post::objects()
    ///     .filter(|f| f.published().eq(false))
    ///     .delete(&pool)
    ///     .await?;
    /// ```
    pub fn delete<'a, E>(self, executor: E) -> impl Future<Output = Result<u64, DjogiError>> + Send
    where
        E: sqlx::Executor<'a, Database = sqlx::Postgres> + Send,
    {
        async move {
            // TASK6:empty_contract — structural-none queryset: no SQL.
            if self.is_empty() {
                return Ok(0);
            }
            let mut qb = build_delete(&self);
            let result = qb
                .build()
                .execute(executor)
                .await
                .map_err(DjogiError::from)?;
            Ok(result.rows_affected())
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
        fn get<'a>(
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
            _id: i64,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create<'a>(
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'a>(
            &self,
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn delete<'a>(
            self,
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'a>(
            &self,
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
    }

    #[test]
    fn field_ref_set_builds_assignment_with_projected_value() {
        let f: FieldRef<Fake, i32> = FieldRef::new("view_count");
        let a = f.set(42i32);
        assert_eq!(a.column(), "view_count");
        assert!(matches!(a.value(), FilterValue::I32(42)));
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
