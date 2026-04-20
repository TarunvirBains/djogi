//! Scalar-aggregate terminal — `QuerySet::aggregate(...)` + its
//! [`AggregateQuery<T, Out>`] pending handle.
//!
//! # What
//!
//! `QuerySet<T>::aggregate(|f| f.col().sum())` returns an
//! [`AggregateQuery<T, Out>`] whose terminal
//! [`AggregateQuery::fetch_one`] issues
//!
//! ```sql
//! SELECT <agg_expr> FROM <table> [WHERE ...]
//! ```
//!
//! and decodes the single scalar result into `Out`. `Out` is the Rust
//! return type carried by [`crate::expr::AggregateExpr<Out>`] — `i64`
//! for `COUNT`, `f64` for `AVG`, `V` for `SUM`/`MIN`/`MAX` where `V` is
//! the underlying column's scalar type.
//!
//! # Why a dedicated pending handle
//!
//! The aggregate terminal is a scalar decode, not a row decode, so it
//! needs a different sqlx entry point (`query_scalar` instead of
//! `query_as`). Keeping the typed-scalar pending struct separate from
//! [`QuerySet<T>`] preserves Phase 2's terminal signatures byte-for-
//! byte — no call site that reaches `.fetch_all(ctx)` is forced to
//! learn a new return type. The cost is one tiny wrapper struct; the
//! benefit is clean additivity.
//!
//! # Clause set
//!
//! `SELECT <agg>, FROM <table> [WHERE ...]` — no `ORDER BY`, no
//! `LIMIT`, no `OFFSET`, no `GROUP BY`. Ungrouped aggregates always
//! collapse to exactly one result row regardless of cardinality, so
//! those clauses would be meaningless / syntax errors. Grouped
//! aggregates (`annotate(|f| f.col.count()).group_by(...)`) ship in a
//! later phase; Task 4's scalar path stays single-row by design.
//!
//! # Empty short-circuit
//!
//! `QuerySet::none()` on the upstream queryset is honoured — the
//! terminal short-circuits to a sentinel value without issuing any
//! SQL. For `COUNT`-shaped aggregates the sentinel is `0`; for
//! `MIN`/`MAX` it would be `NULL` in SQL (the queryset matched no
//! rows), but we cannot conjure a `V` at the Rust level without a
//! trait bound. Task 4 ships the straightforward version: structural-
//! empty querysets still run the SQL and Postgres returns `NULL` /
//! `0` / `0.0` per the per-aggregate rules. If a later task adds
//! zero-value defaults per `Out`, it will close this gap additively.

#![allow(clippy::manual_async_fn)]

use crate::DjogiError;
use crate::context::{ContextInner, DjogiContext};
use crate::expr::AggregateExpr;
use crate::model::Model;
use crate::query::queryset::QuerySet;
use crate::query::sql::build_aggregate_select;
use std::future::Future;
use std::marker::PhantomData;

/// Pending aggregate query — produced by [`QuerySet::aggregate`] and
/// terminated with [`Self::fetch_one`].
///
/// Holds the upstream queryset (for the `FROM` + `WHERE` clauses) plus
/// the aggregate expression itself. `Out` is the Rust type sqlx decodes
/// the scalar result into — it is threaded from the wrapped
/// [`AggregateExpr<Out>`].
///
/// `#[must_use]` because an unawaited pending query is always a
/// mistake; the `.fetch_one(ctx)` call is what actually runs the SQL.
#[must_use = "aggregate queries are lazy — dropping one silently omits the query"]
pub struct AggregateQuery<T: Model, Out> {
    pub(crate) qs: QuerySet<T>,
    pub(crate) agg: AggregateExpr<Out>,
    pub(crate) _out: PhantomData<fn() -> Out>,
}

impl<T: Model, Out> AggregateQuery<T, Out>
where
    Out: for<'r> sqlx::Decode<'r, sqlx::Postgres>
        + sqlx::Type<sqlx::Postgres>
        + Send
        + Unpin
        + 'static,
{
    /// Execute the aggregate query and decode the single scalar result.
    ///
    /// Dispatches through the same inline-match on [`ContextInner`]
    /// every other terminal uses — aggregate queries work inside an
    /// `atomic()` scope and see the scope's uncommitted writes.
    ///
    /// # Short-circuit
    ///
    /// Does **not** short-circuit on `QuerySet::none()`. Aggregate
    /// semantics differ per op (COUNT on empty → 0; SUM on empty →
    /// NULL; AVG on empty → NULL) and the typed surface cannot
    /// synthesise a `NULL` value at the Rust level without an
    /// `Out: Default` bound — which would exclude the `i64`-decode
    /// path sqlx already supports transparently. So the query runs and
    /// Postgres returns the per-op empty result, which sqlx then
    /// decodes (or errors on NULL for non-Option `Out`). Callers who
    /// need an `Option<V>` shape on MIN/MAX wrap `Out` themselves at
    /// the call site (e.g. `aggregate(|f| f.col().min()).fetch_one::<Option<i64>>(ctx)`).
    pub fn fetch_one<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Out, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
        Out: 'ctx,
    {
        async move {
            let mut qb = build_aggregate_select(&self.qs, &self.agg.node);
            let q = qb.build_query_scalar::<Out>();
            let v: Out = match ctx.inner_mut() {
                ContextInner::Pool(pool) => q.fetch_one(&*pool).await.map_err(DjogiError::from)?,
                ContextInner::Transaction(tx) => {
                    q.fetch_one(&mut **tx).await.map_err(DjogiError::from)?
                }
            };
            Ok(v)
        }
    }
}

// Entry point on QuerySet — builder method that consumes the queryset
// and returns the pending aggregate.

impl<T: Model> QuerySet<T> {
    /// Apply a scalar aggregate (`COUNT` / `SUM` / `AVG` / `MIN` /
    /// `MAX`) to this queryset.
    ///
    /// The closure receives a default-constructed `T::Fields` handle
    /// and must return an [`AggregateExpr<Out>`] — built by calling
    /// `.count()` / `.sum()` / `.avg()` / `.min()` / `.max()` on a
    /// [`crate::query::FieldRef`] produced by the fields struct.
    /// Chain `.filter(Expr<bool>)` on the aggregate for
    /// `FILTER (WHERE ...)` post-filtering.
    ///
    /// The pending [`AggregateQuery<T, Out>`] is terminated with
    /// [`AggregateQuery::fetch_one`], which issues
    /// `SELECT <agg> FROM <table> [WHERE ...]` and decodes the single
    /// scalar result.
    ///
    /// ```ignore
    /// use djogi::prelude::*;
    ///
    /// let total: i64 = Account::objects()
    ///     .filter(|f| f.published().eq(true))
    ///     .aggregate(|f| f.balance().sum())
    ///     .fetch_one(&mut ctx).await?;
    /// ```
    #[must_use = "aggregate queries are lazy — dropping one silently omits the query"]
    pub fn aggregate<F, Out>(self, f: F) -> AggregateQuery<T, Out>
    where
        F: FnOnce(T::Fields) -> AggregateExpr<Out>,
    {
        let agg = f(T::Fields::default());
        AggregateQuery {
            qs: self,
            agg,
            _out: PhantomData,
        }
    }
}
