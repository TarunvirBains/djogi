//! Scalar-aggregate terminal — `QuerySet::aggregate(...)` + its
//! [`AggregateQuery<T, Out, K>`] pending handle.
//! # What
//! `QuerySet<T>::aggregate(|f| f.col().sum())` returns an
//! [`AggregateQuery<T, Out, K>`] whose terminal
//! [`AggregateQuery::fetch_one`] issues
//! ```sql
//! SELECT <agg_expr> FROM <table> [WHERE ...]
//! ```
//! and decodes the single scalar result into `Out`. `Out` is the Rust
//! return type carried by [`crate::expr::AggregateExpr<Out, K>`] — `i64`
//! for `COUNT`, `f64` for `AVG`, `V` for `SUM`/`MIN`/`MAX` where `V` is
//! the underlying column's scalar type. `K` is the modifier-family
//! marker; it defaults to [`crate::expr::ValueAgg`] and is inferred
//! from the aggregate the closure returns so non-value families
//! (`PERCENTILE_CONT`, `RANK_OF`, `GROUPING`, etc.) flow through this
//! terminal without explicit annotation.
//! # Why a dedicated pending handle
//! The aggregate terminal is a scalar decode, not a row decode, so it
//! needs a different entry point from the row terminals. Keeping the
//! typed-scalar pending struct separate from [`QuerySet<T>`] preserves
//! The terminal signatures byte-for-byte — no call site that
//! reaches `.fetch_all(ctx)` is forced to learn a new return type. The
//! cost is one tiny wrapper struct; the benefit is clean additivity.
//! # Clause set
//! `SELECT <agg>, FROM <table> [WHERE ...]` — no `ORDER BY`, no
//! `LIMIT`, no `OFFSET`, no `GROUP BY`. Ungrouped aggregates always
//! collapse to exactly one result row regardless of cardinality, so
//! those clauses would be meaningless / syntax errors. Grouped
//! aggregates (`annotate(|f| f.col.count()).group_by(...)`) ship in a
//! later phase; Task 4's scalar path stays single-row by design.
//! # Empty short-circuit
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
use crate::context::DjogiContext;
use crate::expr::AggregateExpr;
use crate::expr::aggregate::{KindEvidence, ValueAgg};
use crate::model::Model;
use crate::pg::accumulator::as_params;
use crate::query::queryset::QuerySet;
use crate::query::sql::build_aggregate_select;
use std::future::Future;
use std::marker::PhantomData;

/// Pending aggregate query — produced by [`QuerySet::aggregate`] and
/// terminated with [`Self::fetch_one`].
/// Holds the upstream queryset (for the `FROM` + `WHERE` clauses) plus
/// the aggregate expression itself. `Out` is the Rust type the driver
/// decodes the scalar result into; `K` is the aggregate's modifier
/// family ([`crate::expr::ValueAgg`] / [`crate::expr::MetadataAgg`] /
/// [`crate::expr::OrderedSetAgg`] / [`crate::expr::HypotheticalSetAgg`]),
/// threaded from the wrapped [`AggregateExpr<Out, K>`].
/// `K` defaults to [`crate::expr::ValueAgg`] so pre-#89 call sites that
/// passed a value aggregate (`f.col().sum()`, `f.col().count()`, etc.)
/// keep working without explicit annotation. Non-value families
/// (`f.col().percentile_cont(0.5)`, `f.col().rank_of(7_500)`,
/// `f.col().grouping()`) infer `K` from the returned aggregate's kind
/// marker.
/// `#[must_use]` because an unawaited pending query is always a
/// mistake; the `.fetch_one(ctx)` call is what actually runs the SQL.
#[must_use = "aggregate queries are lazy — dropping one silently omits the query"]
pub struct AggregateQuery<T: Model, Out, K = ValueAgg> {
    pub(crate) qs: QuerySet<T>,
    pub(crate) agg: AggregateExpr<Out, K>,
    pub(crate) _out: PhantomData<fn() -> Out>,
}

impl<T: Model, Out, K> AggregateQuery<T, Out, K>
where
    K: KindEvidence,
    Out: for<'a> postgres_types::FromSql<'a> + Send + Unpin + 'static,
{
    /// Execute the aggregate query and decode the single scalar result.
    /// Dispatches through the context's execution helpers — aggregate
    /// queries work inside an `atomic()` scope and see the scope's
    /// uncommitted writes.
    /// # Short-circuit
    /// Does **not** short-circuit on `QuerySet::none()`. Aggregate
    /// semantics differ per op (COUNT on empty → 0; SUM on empty →
    /// NULL; AVG on empty → NULL) and the typed surface cannot
    /// synthesise a `NULL` value at the Rust level without an
    /// `Out: Default` bound — which would exclude the `i64`-decode
    /// path. So the query runs and Postgres returns the per-op empty
    /// result, which is then decoded (or errors on NULL for non-Option
    /// `Out`). Callers who need an `Option<V>` shape on MIN/MAX wrap
    /// `Out` themselves at the call site.
    pub fn fetch_one<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Out, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
        Out: 'ctx,
        K: 'ctx,
    {
        async move {
            // Validate DISTINCT modifier combinations before building SQL
            // rejected combos (COUNT(*) + DISTINCT, STRING_AGG + DISTINCT)
            // surface as DjogiError::UnsupportedAggregate rather than a
            // cryptic Postgres syntax error.
            crate::expr::sql::check_aggregate_legality(&self.agg.node)?;
            let acc = build_aggregate_select(&self.qs, &self.agg.node)
                .map_err(crate::DjogiError::from)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let row = ctx.query_one(&sql, &params).await?;
            let v: Out = row.try_get(0).map_err(DjogiError::from)?;
            Ok(v)
        }
    }
}

// Entry point on QuerySet — builder method that consumes the queryset
// and returns the pending aggregate.

impl<T: Model> QuerySet<T> {
    /// Apply a scalar aggregate to this queryset.
    /// The closure receives a default-constructed `T::Fields` handle
    /// and must return an [`AggregateExpr<Out, K>`]. The returned
    /// aggregate can be any kind: value aggregates (`COUNT` / `SUM` /
    /// `AVG` / `MIN` / `MAX`, etc., returning `AggregateExpr<Out>`),
    /// metadata aggregates (`GROUPING`, returning
    /// `AggregateExpr<i32, MetadataAgg>`), ordered-set aggregates
    /// (`PERCENTILE_CONT` / `PERCENTILE_DISC` / `MODE`, returning
    /// `AggregateExpr<Out, OrderedSetAgg>`), or hypothetical-set
    /// aggregates (`RANK_OF` / `DENSE_RANK_OF` / `PERCENT_RANK_OF` /
    /// `CUME_DIST_OF`, returning `AggregateExpr<Out, HypotheticalSetAgg>`).
    /// `K` is inferred from the aggregate the closure returns. Chain
    /// `.filter(Expr<bool>)` on value aggregates for
    /// `FILTER (WHERE ...)` post-filtering; the per-kind impl blocks
    /// on `AggregateExpr` enforce which modifiers are legal per family.
    /// The pending [`AggregateQuery<T, Out, K>`] is terminated with
    /// [`AggregateQuery::fetch_one`], which issues
    /// `SELECT <agg> FROM <table> [WHERE ...]` and decodes the single
    /// scalar result.
    /// ```ignore
    /// use djogi::prelude::*;
    ///
    /// // Value aggregate — `K = ValueAgg` (default).
    /// let total: i64 = Account::objects()
    ///     .filter(|f| f.published().eq(true))
    ///     .aggregate(|f| f.balance().sum())
    ///     .fetch_one(&mut ctx).await?;
    ///
    /// // Ordered-set aggregate — `K = OrderedSetAgg` inferred.
    /// let p95: f64 = Request::objects()
    ///     .aggregate(|r| r.latency_ms().percentile_cont(0.95))
    ///     .fetch_one(&mut ctx).await?;
    /// ```
    #[must_use = "aggregate queries are lazy — dropping one silently omits the query"]
    pub fn aggregate<F, Out, K>(self, f: F) -> AggregateQuery<T, Out, K>
    where
        F: FnOnce(T::Fields) -> AggregateExpr<Out, K>,
        K: KindEvidence,
    {
        let agg = f(T::Fields::default());
        AggregateQuery {
            qs: self,
            agg,
            _out: PhantomData,
        }
    }
}
