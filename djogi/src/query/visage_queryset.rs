//! `VisageQuerySet<V>` — read-only queryset over a visage projection.
//! # What
//! Each emitted visage struct gets a queryset entry point: `UserPublic::filter(|f| ...)`.
//! Because visages are projections, not tables, they do not implement [`Model`]
//! (the existing `QuerySet<T: Model>` carries a `T: Model` bound that visages
//! cannot satisfy without misrepresenting the column shape). Instead, the
//! `#[model]` macro emits per-visage entry points that build a
//! [`VisageQuerySet<V>`] — a sibling type that mirrors the read-only subset of
//! [`QuerySet`]'s surface and emits a SELECT narrowed to the visage's exposed
//! column list.
//! Predicate typing matches the source model, not the visage projection:
//! `VisageQuerySet<V>` stores a `Q<V::Model>` filter tree and every
//! filter-builder entry point accepts any [`IntoQ<V::Model>`](crate::query::IntoQ)
//! payload. In practice that means ordinary legacy [`Condition`] leaves,
//! mixed [`Predicate<V::Model>`](crate::query::Predicate) trees, portable
//! field predicates, and codec-gated `Q<_>` values all compose through the
//! same `filter(...)` surface.
//! # Why a sibling type instead of relaxing `QuerySet<T>`'s bound
//! `QuerySet<T: Model>` participates in dozens of code paths (`prefetch`,
//! `select_related`, `for_update`, `annotate`, JSONB / spatial / FTS
//! emitters) that all assume `T::table_name()`, `T::Pk`, and `T::Fields`
//! are available. Generalising the struct over both shapes would either
//! force every helper to dispatch on whether `T` is a model or a visage
//! (an open-ended split that grows every time a new visage-only feature
//! lands) or require visages to fake a `Model` impl with non-matching
//! column shape — which is exactly the leak this task is meant to plug.
//! `VisageQuerySet<V>` keeps the read-only path narrow and the model
//! path unchanged.
//! # Read-only surface (no `bulk_create` / `save` / `delete`)
//! Visages are **projections**: they discard non-exposed columns and may
//! embed peer projections for relations. A round-trip insert from a
//! visage cannot reconstruct the source row faithfully (the dropped
//! columns are irrecoverable, and embedded peers may themselves be
//! lossy). Visage-side write methods would therefore either silently
//! corrupt rows or require complex re-hydration logic that defeats the
//! point. Compile-time enforcement falls out of method absence: the
//! visage struct has no `bulk_create` / `save` / `delete` methods, and
//! `VisageQuerySet<V>` only exposes read terminals.
//! # SQL narrowing
//! `VisageQuerySet<V>` carries a pre-rendered `projection_list:
//! &'static str` — the macro renders the visage's SELECT projection
//! once at compile time (column entries pass through verbatim;
//! derived entries from #231 render as
//! `(<sql>) AS <alias>`) and the queryset splices the rendered
//! string directly into the SELECT slot. The string is emitted in
//! lockstep with the visage's positional `FromPgRow` decoder so row
//! ordinals align with struct field order. The narrowed projection
//! is the load-bearing detail: dropped columns (`password_hash`,
//! `email` on the `public` scope) are absent from the SELECT
//! projection and from any `RETURNING` shape — they cannot leak
//! through this surface.
//! # Why RPITIT (not `async fn`)
//! Every terminal returns `impl Future<Output = ...> + Send` rather
//! than using bare `async fn`. The explicit `+ Send` bound matches the
//! model-side `QuerySet` terminals and guarantees the returned future
//! can be `.await`ed across task boundaries. `clippy::manual_async_fn`
//! fires on this pattern; the lint is allowed at the module level
//! because the explicit-bound form is the deliberate choice.
#![allow(clippy::manual_async_fn)]

use crate::DjogiError;
use crate::context::DjogiContext;
use crate::pg::accumulator::{SqlAccumulator, as_params};
use crate::pg::decode::{FromPgRow, try_get_scalar};
use crate::query::order::OrderExpr;
use crate::query::portable::SqlEmitContext;
use crate::query::sql::{emit_q, q_is_vacuously_true};
use crate::query::{IntoQ, Q};
use crate::visage::DjogiVisage;
use postgres_types::ToSql;
use std::future::Future;
use std::marker::PhantomData;

/// Read-only queryset over a visage projection.
/// Constructed by the `#[model]`-emitted `V::filter(...)` entry point.
/// Builder methods (`order_by`, `limit`, `offset`) consume `self` and
/// return `Self`, mirroring [`QuerySet`]'s immutable-by-convention
/// composition style. Terminal methods (`fetch_all`, `fetch_one`,
/// `first`, `count`, `exists`) consume the queryset and execute SQL
/// against a caller-supplied `&mut DjogiContext`.
/// `V` is the visage type. The macro pairs each visage with its source
/// model via the `DjogiVisageOf<M>` seal; `VisageQuerySet<V>` does not
/// expose that pairing because the read path doesn't need it — the
/// table name and column list are already baked into the queryset
/// state at construction time.
/// [`QuerySet`]: crate::query::QuerySet
// Visages do not implement tree-query traits.
// `VisageQuerySet<V>` intentionally has no `tree_descendants` /
// `tree_ancestors` / `full_ancestors` methods — recursive walks need
// the full row materialised at every step, which is the column set
// a visage narrows away. See `crate::query::recursive` module docs
// for the full rationale and the future-work pointer.
pub struct VisageQuerySet<V: DjogiVisage> {
    /// Source-model SQL table name. Captured from the macro at
    /// construction time so the queryset has no `T: Model` bound.
    pub(crate) table: &'static str,
    /// Pre-rendered SELECT projection list. Splices directly into the
    /// SELECT slot at query time — no runtime walk over a column
    /// slice. For column-only visages this is just the comma-joined
    /// column names; for visages with derived entries the macro
    /// renders the entries as `(<sql>) AS <alias>` and joins the
    /// list at compile time. #231.
    pub(crate) projection_list: &'static str,
    /// Accumulated filter tree. Starts as [`Q::always_true`].
    pub(crate) condition: Q<V::Model>,
    /// Ordering expressions in emission order. `order_by` appends.
    pub(crate) ordering: Vec<OrderExpr>,
    /// SQL `LIMIT` — `None` means no limit.
    pub(crate) limit: Option<i64>,
    /// SQL `OFFSET` — `None` means no offset.
    pub(crate) offset: Option<i64>,
    /// Covariant `V` tag; never owns or borrows a `V`.
    _visage: PhantomData<fn() -> V>,
}

/// Single exposed visage column selector for subquery projection.
#[must_use = "visage columns are inert until projected through selecting()"]
pub struct VisageColumn<V: DjogiVisage, U> {
    pub(crate) column: &'static str,
    __sealed: crate::visage::VisageColumnToken,
    _v: PhantomData<fn() -> V>,
    _u: PhantomData<fn() -> U>,
}

impl<V: DjogiVisage, U> Clone for VisageColumn<V, U> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<V: DjogiVisage, U> Copy for VisageColumn<V, U> {}

impl<V: DjogiVisage, U> std::fmt::Debug for VisageColumn<V, U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VisageColumn")
            .field("column", &self.column)
            .finish()
    }
}

impl<V: DjogiVisage, U> VisageColumn<V, U> {
    #[doc(hidden)]
    pub fn __new_for_visage_column(
        column: &'static str,
        token: crate::visage::VisageColumnToken,
    ) -> Self {
        crate::ident::assert_plain_ident(column, "visage_column");
        assert!(
            <V as crate::visage::DjogiVisage>::COLUMNS.contains(&column),
            "djogi::query::VisageColumn: column {column:?} is not a member of visage `{}`'s exposed COLUMNS",
            std::any::type_name::<V>(),
        );
        Self {
            column,
            __sealed: token,
            _v: PhantomData,
            _u: PhantomData,
        }
    }
}

/// Single-column visage subquery ready for `IN` / quantified embedding.
#[must_use = "a VisageSubquery is lazy — embed it in a field predicate or it has no effect"]
pub struct VisageSubquery<V: DjogiVisage, U> {
    table: &'static str,
    select_column: &'static str,
    condition: Q<V::Model>,
    _u: PhantomData<fn() -> U>,
}

impl<V: DjogiVisage, U> std::fmt::Debug for VisageSubquery<V, U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VisageSubquery")
            .field("table", &self.table)
            .field("select_column", &self.select_column)
            .field("condition", &self.condition)
            .finish()
    }
}

/// Typed `EXISTS (...)` predicate built from a visage queryset.
#[must_use = "EXISTS predicates are lazy — drop one and the filter is silently omitted"]
#[derive(Debug, Clone)]
pub struct VisageExists {
    node: crate::expr::node::SubqueryNode,
}

impl VisageExists {
    pub fn new<V: DjogiVisage>(qs: VisageQuerySet<V>) -> Result<Self, DjogiError> {
        reject_subquery_modifiers("VisageExists::new", &qs.ordering, qs.limit, qs.offset)?;
        Ok(Self {
            node: crate::expr::node::SubqueryNode {
                table: qs.table,
                select_column: None,
                where_clause: crate::expr::subquery::__q_to_subquery_opt::<V::Model>(qs.condition),
            },
        })
    }

    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn as_expr(self) -> crate::expr::Expr<bool> {
        crate::expr::Expr::from_node(crate::expr::node::ExprNode::Exists(Box::new(self.node)))
    }

    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn not_exists(self) -> crate::expr::Expr<bool> {
        let expr = self.as_expr();
        crate::expr::Expr::from_node(crate::expr::node::ExprNode::Not(Box::new(expr.node)))
    }
}

impl<V: DjogiVisage, U> VisageSubquery<V, U> {
    #[doc(hidden)]
    pub(crate) fn __into_subquery_node(self) -> crate::expr::node::SubqueryNode {
        crate::expr::node::SubqueryNode {
            table: self.table,
            select_column: Some(self.select_column),
            where_clause: crate::expr::subquery::__q_to_subquery_opt::<V::Model>(self.condition),
        }
    }
}

impl<V: DjogiVisage> Clone for VisageQuerySet<V> {
    fn clone(&self) -> Self {
        VisageQuerySet {
            table: self.table,
            projection_list: self.projection_list,
            condition: self.condition.clone(),
            ordering: self.ordering.clone(),
            limit: self.limit,
            offset: self.offset,
            _visage: PhantomData,
        }
    }
}

impl<V: DjogiVisage> std::fmt::Debug for VisageQuerySet<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VisageQuerySet")
            .field("table", &self.table)
            .field("projection_list", &self.projection_list)
            .field("condition", &self.condition)
            .field("ordering", &self.ordering)
            .field("limit", &self.limit)
            .field("offset", &self.offset)
            .finish()
    }
}

impl<V: DjogiVisage> VisageQuerySet<V> {
    /// Construct a `VisageQuerySet` with the given table and narrowed
    /// projection list.
    /// Internal constructor — called by the `#[model]`-emitted
    /// `V::filter(...)` / `V::order_by(...)` / `V::limit(...)` entry points;
    /// not part of the user-visible API.
    /// The queryset starts with a vacuous root predicate (`Q::always_true()`)
    /// and does **not** seed any proxy `default_filter_condition`. Seeding the
    /// proxy default is the caller's responsibility: the macro-emitted
    /// `V::__new()` wrapper threads the source model's
    /// [`Model::default_filter_condition`](crate::model::Model::default_filter_condition)
    /// in via [`filter`](Self::filter) immediately after this call, matching
    /// what [`QuerySet::new`](crate::query::QuerySet::new) does on the model
    /// side. Any direct caller that bypasses `V::__new()` must apply that
    /// `.filter(...)` itself, or proxy-scoped visages will silently return
    /// rows the proxy filter is meant to exclude.
    /// The `projection_list` is the visage's pre-rendered SELECT
    /// projection — column entries pass through verbatim; derived
    /// entries render as `(<sql>) AS <alias>` and the list is joined
    /// at compile time. The string is emitted by the macro in
    /// lockstep with the visage's positional `FromPgRow` decoder so
    /// row ordinals align with struct field order.
    #[doc(hidden)]
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn new_for_visage(table: &'static str, projection_list: &'static str) -> Self {
        VisageQuerySet {
            table,
            projection_list,
            condition: Q::<V::Model>::always_true(),
            ordering: Vec::new(),
            limit: None,
            offset: None,
            _visage: PhantomData,
        }
    }

    /// AND another predicate onto the accumulated filter tree.
    /// Accepts any [`IntoQ<V::Model>`](crate::query::IntoQ) payload:
    /// legacy [`Condition`](crate::query::internal::Condition),
    /// portable / mixed predicate wrappers, or a pre-built `Q<V::Model>`.
    /// See also: [`QuerySet::filter`](crate::query::QuerySet::filter)
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn filter<P>(mut self, cond: P) -> Self
    where
        P: IntoQ<V::Model>,
    {
        self.condition = and_q_into_q(self.condition, cond);
        self
    }

    /// Append an ordering expression.
    /// Mirrors [`QuerySet::order_by`]'s append semantics — repeated
    /// calls accumulate rather than replace, so library code can add a
    /// stable tiebreaker without clobbering the caller's primary
    /// ordering.
    /// [`QuerySet::order_by`]: crate::query::QuerySet::order_by
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn order_by<I: Into<Vec<OrderExpr>>>(mut self, orderings: I) -> Self {
        self.ordering.extend(orderings.into());
        self
    }

    /// Apply SQL `LIMIT n`. Replaces any prior `limit` value.
    /// Panics if `n > i64::MAX` — Postgres bind type for `LIMIT` is `BIGINT`,
    /// so values above `i64::MAX` cannot round-trip. The check uses
    /// `try_from` (not `debug_assert!`) so release builds also panic
    /// rather than silently truncate.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn limit(mut self, n: u64) -> Self {
        let n = i64::try_from(n)
            .unwrap_or_else(|_| panic!("VisageQuerySet::limit(n = {n}) overflows i64"));
        self.limit = Some(n);
        self
    }

    /// Apply SQL `OFFSET n`. Replaces any prior `offset` value.
    /// Panics if `n > i64::MAX` — see [`Self::limit`] for the rationale.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn offset(mut self, n: u64) -> Self {
        let n = i64::try_from(n)
            .unwrap_or_else(|_| panic!("VisageQuerySet::offset(n = {n}) overflows i64"));
        self.offset = Some(n);
        self
    }

    /// Project this visage queryset to a single exposed column for subquery
    /// embedding.
    pub fn selecting<U>(self, col: VisageColumn<V, U>) -> Result<VisageSubquery<V, U>, DjogiError> {
        reject_subquery_modifiers("selecting", &self.ordering, self.limit, self.offset)?;
        Ok(VisageSubquery {
            table: self.table,
            select_column: col.column,
            condition: self.condition,
            _u: PhantomData,
        })
    }

    /// Render the SQL string this queryset would send to Postgres.
    /// **This is internal-test plumbing — never call this from adopter
    /// code.** The emitted SQL is implementation-detail and has no
    /// stability guarantee across phases. Tests that pin SELECT
    /// narrowing use this hook to assert the textual shape directly
    /// without needing `pg_stat_statements` server-side configuration
    /// (which is unavailable on most Postgres test environments).
    /// The double-underscore prefix is the framework's
    /// `feedback_macro_path_routing` convention for "macro-internal
    /// public-by-necessity surface, do not depend on" — same as
    /// `__make_field_ref_with_path` and friends. The method stays
    /// `pub` (cross-crate test access requires it) but `#[doc(hidden)]`
    /// keeps it out of rustdoc.
    #[doc(hidden)]
    pub fn __sql_for_test(&self) -> String {
        let acc = build_visage_select(self).expect("visage select");
        let (sql, _binds) = acc.into_parts();
        sql
    }
}

/// Build the SELECT SQL + binds for `fetch_all`, `fetch_one`, and `first`.
fn run_all_sql<V: DjogiVisage>(
    qs: &VisageQuerySet<V>,
) -> Result<(String, Vec<Box<dyn ToSql + Sync + Send>>), crate::DjogiError> {
    Ok(build_visage_select(qs)
        .map_err(crate::DjogiError::from)?
        .into_parts())
}

/// Build `SELECT col1, col2, ... FROM <table> [WHERE ...] [ORDER BY ...]
/// [LIMIT $n] [OFFSET $n]` for a visage queryset.
/// The projection comes from the visage's narrowed `columns` slice, NOT
/// from a model descriptor walk. This is the load-bearing difference
/// from the model-side `build_select`: dropped columns never appear in
/// the SELECT regardless of the source model's full column count.
pub(crate) fn build_visage_select<V: DjogiVisage>(
    qs: &VisageQuerySet<V>,
) -> Result<SqlAccumulator, crate::query::portable::PortablePredicateError> {
    let mut acc = SqlAccumulator::new("");
    acc.push_sql("SELECT ");
    // #231 — splice the pre-rendered projection list. For
    // column-only visages this matches the prior `push_csv(columns)`
    // shape byte-for-byte; for visages with derived entries it
    // additionally emits `(<sql>) AS <alias>` segments wherever the
    // macro placed a derived projection.
    acc.push_sql(qs.projection_list);
    acc.push_sql(" FROM ");
    acc.push_sql(qs.table);
    push_visage_tail(&mut acc, qs)?;
    Ok(acc)
}

/// Build `SELECT COUNT(*) FROM <table> [WHERE ...]` for a visage queryset.
/// Ignores `ordering`, `limit`, and `offset` — count is invariant under
/// those clauses. Mirrors the model-side `build_count` shape so the
/// emitted statement is the simplest predicate-matching count Postgres
/// can plan.
pub(crate) fn build_visage_count<V: DjogiVisage>(
    qs: &VisageQuerySet<V>,
) -> Result<SqlAccumulator, crate::query::portable::PortablePredicateError> {
    let mut acc = SqlAccumulator::new("");
    acc.push_sql("SELECT COUNT(*) FROM ");
    acc.push_sql(qs.table);
    push_visage_where(&mut acc, qs)?;
    Ok(acc)
}

/// Build `SELECT EXISTS (SELECT 1 FROM <table> [WHERE ...] LIMIT 1)` for
/// a visage queryset. Mirrors the model-side `build_exists` shape.
pub(crate) fn build_visage_exists<V: DjogiVisage>(
    qs: &VisageQuerySet<V>,
) -> Result<SqlAccumulator, crate::query::portable::PortablePredicateError> {
    let mut acc = SqlAccumulator::new("");
    acc.push_sql("SELECT EXISTS (SELECT 1 FROM ");
    acc.push_sql(qs.table);
    push_visage_where(&mut acc, qs)?;
    acc.push_sql(" LIMIT 1)");
    Ok(acc)
}

/// Emit the `WHERE ...` clause for a visage queryset, if non-vacuous.
/// `emit_condition` borrow-walks and returns
/// `Result<(), PortablePredicateError>`; visage querysets carry pure
/// `Condition` payloads (no portable predicates yet), so the only
/// failure path is an inner expression-IR emit error inside a
/// `Condition::Expr`. Propagate via `Result` so the surrounding
/// terminal can lift the error to `DjogiError::Predicate(_)`.
fn push_visage_where<V: DjogiVisage>(
    acc: &mut SqlAccumulator,
    qs: &VisageQuerySet<V>,
) -> Result<(), crate::query::portable::PortablePredicateError> {
    if !q_is_vacuously_true(&qs.condition) {
        acc.push_sql(" WHERE ");
        emit_q::<V::Model>(acc, &qs.condition, SqlEmitContext::root())?;
    }
    Ok(())
}

/// Tail clauses shared by SELECT variants: `WHERE`, `ORDER BY`, `LIMIT`,
/// `OFFSET`. Visage querysets do not carry select_related / prefetch /
/// row-lock state, so this is shorter than the model-side
/// `push_tail_qualified`.
fn push_visage_tail<V: DjogiVisage>(
    acc: &mut SqlAccumulator,
    qs: &VisageQuerySet<V>,
) -> Result<(), crate::query::portable::PortablePredicateError> {
    push_visage_where(acc, qs)?;

    if !qs.ordering.is_empty() {
        acc.push_sql(" ORDER BY ");
        for (i, o) in qs.ordering.iter().enumerate() {
            if i > 0 {
                acc.push_sql(", ");
            }
            o.emit(acc, None);
        }
    }

    if let Some(n) = qs.limit {
        acc.push_sql(" LIMIT ");
        acc.push_bind(n);
    }
    if let Some(n) = qs.offset {
        acc.push_sql(" OFFSET ");
        acc.push_bind(n);
    }
    Ok(())
}

// ── Row-returning terminals (require `V: FromPgRow`) ───────────────────────

impl<V: DjogiVisage> VisageQuerySet<V>
where
    V: FromPgRow + Send + Unpin + 'static,
{
    /// Execute the query and collect every matching row into a `Vec<V>`.
    /// Mirrors [`QuerySet::fetch_all`] but with a narrow SELECT and the
    /// visage's own positional `FromPgRow` decoder.
    /// [`QuerySet::fetch_all`]: crate::query::QuerySet::fetch_all
    pub fn fetch_all<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Vec<V>, DjogiError>> + Send + 'ctx
    where
        V: 'ctx,
    {
        async move {
            let (sql, binds) = run_all_sql(&self)?;
            let params = as_params(&binds);
            let rows = ctx.query_all(&sql, &params).await?;
            rows.iter().map(|r| V::from_pg_row(r)).collect()
        }
    }

    /// Execute the query and require **exactly one** matching row.
    /// - Zero rows → [`DjogiError::NotFound`].
    /// - Two or more rows → [`DjogiError::MultipleObjects`].
    pub fn fetch_one<'ctx>(
        mut self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<V, DjogiError>> + Send + 'ctx
    where
        V: 'ctx,
    {
        async move {
            // Force LIMIT 2 so we can distinguish single-row success from
            // multi-row failure without a separate COUNT round trip.
            let table = self.table;
            self.limit = Some(2);
            let (sql, binds) = run_all_sql(&self)?;
            let params = as_params(&binds);
            let rows = ctx.query_all(&sql, &params).await?;
            match rows.len() {
                0 => Err(DjogiError::not_found(table)),
                1 => {
                    let row = rows
                        .into_iter()
                        .next()
                        .expect("rows.len() == 1 was just matched");
                    V::from_pg_row(&row)
                }
                n => Err(DjogiError::multiple_objects(table, n)),
            }
        }
    }

    /// Execute the query and return the first matching row, or `None` if
    /// no rows match.
    pub fn first<'ctx>(
        mut self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Option<V>, DjogiError>> + Send + 'ctx
    where
        V: 'ctx,
    {
        async move {
            self.limit = Some(1);
            let (sql, binds) = run_all_sql(&self)?;
            let params = as_params(&binds);
            let rows = ctx.query_all(&sql, &params).await?;
            match rows.into_iter().next() {
                None => Ok(None),
                Some(r) => V::from_pg_row(&r).map(Some),
            }
        }
    }
}

// ── Aggregate terminals (no V: FromPgRow bound) ────────────────────────────

impl<V: DjogiVisage> VisageQuerySet<V> {
    /// Return the count of rows matching the queryset's predicate.
    pub fn count<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<i64, DjogiError>> + Send + 'ctx
    where
        V: 'ctx,
    {
        async move {
            let (sql, binds) = build_visage_count(&self)
                .map_err(DjogiError::from)?
                .into_parts();
            let params = as_params(&binds);
            let row = ctx.query_one(&sql, &params).await?;
            try_get_scalar::<i64>(&row, 0)
        }
    }

    /// Return `true` if at least one row matches the queryset's predicate.
    pub fn exists<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<bool, DjogiError>> + Send + 'ctx
    where
        V: 'ctx,
    {
        async move {
            let (sql, binds) = build_visage_exists(&self)
                .map_err(DjogiError::from)?
                .into_parts();
            let params = as_params(&binds);
            let row = ctx.query_one(&sql, &params).await?;
            try_get_scalar::<bool>(&row, 0)
        }
    }
}

fn and_q_into_q<T: crate::model::Model, A: IntoQ<T>>(current: Q<T>, addition: A) -> Q<T> {
    let addition = addition.into_q();
    if q_is_vacuously_true(&current) {
        addition
    } else if q_is_vacuously_true(&addition) {
        current
    } else {
        current & addition
    }
}

fn reject_subquery_modifiers(
    entry_point: &'static str,
    ordering: &[OrderExpr],
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<(), DjogiError> {
    let offending = if !ordering.is_empty() {
        Some("order_by")
    } else if limit.is_some() {
        Some("limit")
    } else if offset.is_some() {
        Some("offset")
    } else {
        None
    };
    if let Some(modifier) = offending {
        return Err(DjogiError::invalid_subquery_modifier(
            entry_point,
            match modifier {
                "order_by" => {
                    "order_by is not meaningful in a subquery; remove it before embedding"
                }
                "limit" => "limit is not meaningful in a subquery; remove it before embedding",
                "offset" => "offset is not meaningful in a subquery; remove it before embedding",
                other => unreachable!(
                    "reject_subquery_modifiers: unhandled modifier {other:?} — offending selector out of sync"
                ),
            },
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{VisageColumn, VisageExists, VisageQuerySet};
    use crate::model::Model;
    use crate::pg::accumulator::SqlAccumulator;
    use crate::query::SqlEmitContext;
    use crate::query::field::djogi_field_macro_support::__make_djogi_field;

    #[allow(dead_code)]
    struct Src {
        id: i64,
        name: String,
    }

    impl crate::model::__sealed::Sealed for Src {}
    #[allow(clippy::manual_async_fn)]
    impl Model for Src {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "srcs"
        }
        fn pk_value(&self) -> &i64 {
            &self.id
        }
        fn descriptor() -> &'static crate::descriptor::ModelDescriptor {
            unimplemented!()
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: i64,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send + 'ctx
        {
            async { unimplemented!() }
        }
        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send + 'ctx
        {
            async { unimplemented!() }
        }
    }

    struct SrcView;
    impl crate::visage_boundary::private::Sealed<Src> for SrcView {}
    impl crate::visage_boundary::DjogiVisageOf<Src> for SrcView {}
    impl crate::visage::private::Sealed for SrcView {}
    impl crate::visage::DjogiVisage for SrcView {
        type Model = Src;
        const SCOPE: &'static str = "public";
        const COLUMNS: &'static [&'static str] = &["id", "name"];
        const PROJECTIONS: &'static [crate::__private::ProjectionEntry] = &[];
        const PROJECTION_LIST: &'static str = "id, name";
    }

    fn test_col<U>(name: &'static str) -> VisageColumn<SrcView, U> {
        VisageColumn::__new_for_visage_column(name, crate::__private::visage_column_seal::TOKEN)
    }

    fn emit_expr_sql(expr: crate::expr::Expr<bool>) -> String {
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &expr.node, SqlEmitContext::root()).expect("emit");
        acc.sql().trim().to_string()
    }

    #[test]
    fn selecting_builds_visage_subquery_emitting_select_col() {
        let sub = VisageQuerySet::<SrcView>::new_for_visage("srcs", "id, name")
            .selecting(test_col::<i64>("id"))
            .expect("clean queryset");
        let node = sub.__into_subquery_node();
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::__emit_subquery_for_test(&mut acc, &node, SqlEmitContext::root())
            .expect("emit");
        assert_eq!(acc.sql().trim(), "SELECT id FROM srcs");
    }

    #[test]
    fn selecting_rejects_order_by() {
        let name_field =
            __make_djogi_field::<Src, String>("name", |_r: &Src| -> &String { unreachable!() });
        let err = VisageQuerySet::<SrcView>::new_for_visage("srcs", "id, name")
            .order_by(vec![name_field.asc()])
            .selecting(test_col::<i64>("id"))
            .unwrap_err();
        assert!(matches!(
            err,
            crate::DjogiError::InvalidSubqueryModifier { .. }
        ));
        assert!(err.to_string().contains("order_by"), "got: {err}");
    }

    #[test]
    fn selecting_rejects_limit() {
        let err = VisageQuerySet::<SrcView>::new_for_visage("srcs", "id, name")
            .limit(10)
            .selecting(test_col::<i64>("id"))
            .unwrap_err();
        assert!(matches!(
            err,
            crate::DjogiError::InvalidSubqueryModifier { .. }
        ));
        assert!(err.to_string().contains("limit"), "got: {err}");
    }

    #[test]
    fn selecting_rejects_offset() {
        let err = VisageQuerySet::<SrcView>::new_for_visage("srcs", "id, name")
            .offset(5)
            .selecting(test_col::<i64>("id"))
            .unwrap_err();
        assert!(matches!(
            err,
            crate::DjogiError::InvalidSubqueryModifier { .. }
        ));
        assert!(err.to_string().contains("offset"), "got: {err}");
    }

    #[test]
    fn selecting_subquery_narrows_to_one_column_and_keeps_where() {
        let predicate = crate::query::field::FieldRef::<Src, String>::new("name")
            .as_expr()
            .eq(crate::expr::Expr::literal("x".to_string()));
        let sub = VisageQuerySet::<SrcView>::new_for_visage("srcs", "id, name")
            .filter(predicate)
            .selecting(test_col::<i64>("id"))
            .expect("clean queryset");
        let node = sub.__into_subquery_node();
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::__emit_subquery_for_test(&mut acc, &node, SqlEmitContext::root())
            .expect("emit");
        let sql = acc.sql();
        assert!(sql.contains("SELECT id FROM srcs WHERE"), "got: {sql}");
        assert!(!sql.contains("id, name"), "got: {sql}");
        assert!(sql.contains("name = $1"), "got: {sql}");
    }

    #[test]
    fn visage_exists_emits_exists_subquery() {
        let exists = VisageExists::new(VisageQuerySet::<SrcView>::new_for_visage(
            "srcs", "id, name",
        ))
        .expect("clean queryset");
        assert_eq!(
            emit_expr_sql(exists.as_expr()),
            "EXISTS (SELECT 1 FROM srcs)"
        );
    }

    #[test]
    fn visage_exists_not_exists_wraps_in_not() {
        let exists = VisageExists::new(VisageQuerySet::<SrcView>::new_for_visage(
            "srcs", "id, name",
        ))
        .expect("clean queryset");
        assert_eq!(
            emit_expr_sql(exists.not_exists()),
            "NOT (EXISTS (SELECT 1 FROM srcs))"
        );
    }

    #[test]
    fn visage_exists_rejects_order_by() {
        let name_field =
            __make_djogi_field::<Src, String>("name", |_r: &Src| -> &String { unreachable!() });
        let err = VisageExists::new(
            VisageQuerySet::<SrcView>::new_for_visage("srcs", "id, name")
                .order_by(vec![name_field.asc()]),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            crate::DjogiError::InvalidSubqueryModifier { .. }
        ));
        assert!(err.to_string().contains("order_by"), "got: {err}");
    }

    #[test]
    fn visage_exists_rejects_limit() {
        let err = VisageExists::new(
            VisageQuerySet::<SrcView>::new_for_visage("srcs", "id, name").limit(10),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            crate::DjogiError::InvalidSubqueryModifier { .. }
        ));
        assert!(err.to_string().contains("limit"), "got: {err}");
    }

    #[test]
    fn visage_exists_rejects_offset() {
        let err = VisageExists::new(
            VisageQuerySet::<SrcView>::new_for_visage("srcs", "id, name").offset(5),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            crate::DjogiError::InvalidSubqueryModifier { .. }
        ));
        assert!(err.to_string().contains("offset"), "got: {err}");
    }
}
