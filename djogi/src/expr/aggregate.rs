//! Aggregate expressions — the typed surface for `COUNT` / `SUM` / `AVG`
//! / `MIN` / `MAX` on a [`FieldRef`].
//!
//! # What
//!
//! [`AggregateExpr<Out>`] is a `PhantomData<fn() -> Out>`-tagged wrapper
//! around the [`ExprNode::Aggregate`] node. `Out` is the Rust type the
//! aggregate decodes to at fetch time:
//!
//! | Aggregate        | `Out`                                  |
//! |------------------|----------------------------------------|
//! | `count()`        | `i64`                                  |
//! | `count_star()`   | `i64`                                  |
//! | `sum()`          | `V` (column's numeric type)            |
//! | `avg()`          | `f64`                                  |
//! | `min()` / `max()`| `V` (column's type)                    |
//!
//! Every aggregate composes with the expression IR's existing walk — an
//! [`AggregateExpr<Out>`] holds a plain [`ExprNode::Aggregate`] node and
//! the emitter in [`super::sql::emit_expr`] lowers it to the matching
//! Postgres keyword + optional `FILTER (WHERE ...)` tail.
//!
//! # Why typed `Out`
//!
//! The scalar terminal ([`crate::query::aggregate::AggregateQuery::fetch_one`])
//! and the per-column decode on [`crate::query::annotate::AnnotatedQuerySet`]
//! both drive `tokio_postgres::Client::query_one` / `row.get::<_, Out>(..)`
//! — the decoder needs to know the Rust type up front. `Out` is that pin: it
//! captures whatever the aggregate returns so the SELECT-list builder
//! never needs runtime type reflection. No `AggregateExpr<Any>` — the
//! compile-time bound is the whole point.
//!
//! # Bounds on `min` / `max`
//!
//! Rust's `Ord` is the natural "orderable" trait, but `f32` / `f64` do
//! not implement it (NaN makes total order impossible in Rust). Postgres
//! happily runs `MIN`/`MAX` on both integer and floating-point columns,
//! so the typed surface gates `min()` / `max()` on `postgres_types::FromSql`
//! bounds rather than Rust `Ord`. Any column whose value type decodes
//! from a Postgres scalar (`V: for<'r> postgres_types::FromSql<'r>`)
//! can be aggregated — that covers `i16`,
//! `i32`, `i64`, `f32`, `f64`, `Decimal`, `time::OffsetDateTime`,
//! `time::Date`, `String`, and the HeeRanjID PK types.
//!
//! # Chaining `.filter(...)`
//!
//! `AggregateExpr::filter` attaches a `FILTER (WHERE <cond>)` clause.
//! Calling `.filter(...)` twice on the same aggregate **overwrites**
//! the previous filter; users compose multi-predicate filters with the
//! expression IR's `and_with` / `or_with` helpers before handing the
//! result to `.filter(...)`. This matches the `QuerySet::limit(n)`
//! pattern where the last call wins — simplest to reason about,
//! easiest to document.
//!
//! # Where
//!
//! - [`super::node::ExprNode::Aggregate`] / [`super::node::AggOp`] — the
//!   untyped payload.
//! - [`super::sql::emit_expr`] — renders the SQL tokens.
//! - [`crate::query::aggregate::AggregateQuery`] — scalar terminal.
//! - [`crate::query::annotate::AnnotatedQuerySet`] — typed-tuple
//!   terminal that embeds aggregates in the SELECT list alongside `T::*`.

use crate::expr::Expr;
use crate::expr::arithmetic::Numeric;
use crate::expr::node::{AggOp, ExprNode};
use crate::model::Model;
use crate::query::field::FieldRef;
use std::marker::PhantomData;

/// Typed aggregate expression — the result of `f.col().count()`,
/// `.sum()`, `.avg()`, `.min()`, `.max()`.
///
/// Carries an [`ExprNode::Aggregate`] payload plus a `PhantomData<fn() ->
/// Out>` tag pinning the Rust decode type. `#[must_use]` because a
/// dropped aggregate is usually a mistake — the user likely meant to
/// feed it into [`crate::query::QuerySet::aggregate`] or
/// [`crate::query::QuerySet::annotate`].
///
/// `Clone + Debug` because the underlying [`ExprNode`] already is —
/// copies are cheap; deep aggregate trees are rare because aggregates
/// bottom out at a column reference or a small arithmetic sub-tree.
#[must_use = "aggregates are lazy — dropping one silently omits the column"]
#[derive(Clone, Debug)]
pub struct AggregateExpr<Out> {
    pub(crate) node: ExprNode,
    pub(crate) _out: PhantomData<fn() -> Out>,
}

impl<Out> AggregateExpr<Out> {
    /// Crate-private constructor. The typed aggregate methods on
    /// [`FieldRef`] are the supported entry points; downstream code
    /// cannot fabricate an arbitrarily-typed aggregate by smuggling in
    /// a raw [`ExprNode`].
    pub(crate) fn from_node(node: ExprNode) -> Self {
        AggregateExpr {
            node,
            _out: PhantomData,
        }
    }

    /// Attach a `FILTER (WHERE <cond>)` clause to this aggregate.
    ///
    /// Postgres runs the filter inside the aggregate's per-row scan —
    /// rows where `cond` evaluates to false do not contribute to the
    /// aggregate. This is the idiomatic shape for Django-style
    /// "count of rows where X" queries without an additional WHERE
    /// round-trip on the outer query.
    ///
    /// # Overwrite semantics
    ///
    /// Calling `.filter(...)` twice on the same aggregate replaces the
    /// previous filter. Users who need compound filters build them
    /// with the expression IR (`f.col().as_expr().lt(..).and_with(..)`
    /// in Phase 5 once logical `and_with` lands on `Expr<bool>`, or a
    /// nested `Expr<bool>` composition in Phase 4) before chaining
    /// `.filter(...)`. The last call wins, matching
    /// [`crate::query::QuerySet::limit`]'s pattern.
    pub fn filter(mut self, cond: Expr<bool>) -> Self {
        // The Aggregate variant is the only shape `AggregateExpr`
        // ever wraps — constructed exclusively via `from_node(...)`
        // with a fresh `Aggregate { .. }` node in the inherent `count`
        // / `sum` / etc. builders on `FieldRef`. The `if let` is
        // defensive: a debug_assert would panic on a mismatch, but
        // since `AggregateExpr::from_node` is crate-private the
        // match is guaranteed to take the Aggregate arm in practice.
        if let ExprNode::Aggregate { filter, .. } = &mut self.node {
            *filter = Some(Box::new(cond.node));
        }
        self
    }
}

// ── COUNT ─────────────────────────────────────────────────────────────
//
// `count` is available on every `FieldRef<M, V>` regardless of `V`,
// because Postgres `COUNT(col)` works on any column type (it counts
// non-null values). `count_star` is an associated function because it
// does not need a column reference — users call it as
// `AggregateExpr::<i64>::count_star()` or, from a field-closure context,
// build it manually by reaching into the `ExprNode` (which they cannot
// — the enum is crate-private). Task 4 ships `.count()` only; the
// `COUNT(*)` variant is exposed through `FieldRef::count_star()` as an
// inherent method that uses any FieldRef to satisfy the receiver but
// renders as `COUNT(*)`.

impl<M: Model, V> FieldRef<M, V> {
    /// `COUNT(column)` — returns `i64`.
    ///
    /// Counts rows where the column is non-null. For a total row count
    /// that ignores NULL status, use [`FieldRef::count_star`] (which
    /// emits `COUNT(*)`).
    ///
    /// `COUNT` in Postgres always returns `BIGINT`, which decodes
    /// directly into `i64` — no cast needed.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn count(self) -> AggregateExpr<i64> {
        AggregateExpr::from_node(ExprNode::Aggregate {
            op: AggOp::Count,
            arg: Box::new(ExprNode::Field {
                column: self.column(),
            }),
            filter: None,
            cast_to: None,
            distinct: false,
            window: None,
        })
    }

    /// `COUNT(*)` — returns `i64`.
    ///
    /// Counts every row in the (grouped) relation, including those
    /// where every column is NULL. Routes through a dedicated
    /// [`AggOp::CountStar`] variant rather than
    /// `ExprNode::Field { column: "*" }` so the bare `*` never
    /// reaches the identifier-validation pass in
    /// [`crate::ident::assert_plain_ident`] nor the column-
    /// qualification pass that select_related adds.
    ///
    /// `FieldRef<M, V>` is the receiver because `AggregateExpr`
    /// constructors live on `FieldRef` by convention — the receiver's
    /// `column()` is **not** used for `COUNT(*)` (the emitter ignores
    /// the `arg` slot on this variant); it only gives the method a
    /// natural call site inside a field closure.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn count_star(self) -> AggregateExpr<i64> {
        // `arg` is a placeholder — the emitter renders `COUNT(*)` and
        // ignores this slot on the CountStar branch. We still carry a
        // concrete `ExprNode` (not `Option<Box<ExprNode>>`) to keep
        // the variant layout uniform across all AggOps.
        AggregateExpr::from_node(ExprNode::Aggregate {
            op: AggOp::CountStar,
            arg: Box::new(ExprNode::Field {
                column: self.column(),
            }),
            filter: None,
            cast_to: None,
            distinct: false,
            window: None,
        })
    }
}

// ── SUM / AVG ─────────────────────────────────────────────────────────
//
// Gated on the sealed `Numeric` trait from `expr::arithmetic` — same
// seal that gates `+` / `-` / `*` / `/` on `Expr<T>`. Phase 4 ships
// `i16 / i32 / i64 / f32 / f64`; `Decimal` extends the trait in Phase 5.

impl<M: Model, V: Numeric> FieldRef<M, V> {
    /// `SUM(column)` — returns `V`.
    ///
    /// Sums non-null values of the column. Gated on the sealed
    /// [`Numeric`] trait so only framework-blessed numeric types
    /// compose — `sum` on a `String` column is a compile error, not a
    /// runtime SQL error.
    ///
    /// # Postgres widening vs `Out = V`
    ///
    /// Postgres widens integer sums — `SUM(BIGINT)` returns `NUMERIC`,
    /// `SUM(SMALLINT)` returns `BIGINT`. The typed surface keeps
    /// `Out = V` for ergonomics (most call sites sum into the same
    /// scalar type they declared on the column), and the emitter
    /// narrows the result back with an explicit `::<V::SUM_CAST>`
    /// cast so the decoder can return `V` directly.
    ///
    /// This means a sum that overflows the original column's range
    /// raises a `numeric_value_out_of_range` error at query time —
    /// Postgres refuses to truncate on the narrowing cast. That is
    /// deliberate: silent truncation would be worse than an error.
    /// Users aggregating values that exceed `V::MAX` should declare a
    /// larger column type or use `ctx.raw_scalar` for a `NUMERIC` /
    /// `Decimal` decode; Phase 5's `Decimal` `Numeric` impl is the
    /// framework-supported path for precision-critical sums.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn sum(self) -> AggregateExpr<V> {
        AggregateExpr::from_node(ExprNode::Aggregate {
            op: AggOp::Sum,
            arg: Box::new(ExprNode::Field {
                column: self.column(),
            }),
            filter: None,
            // `V::SUM_CAST` — associated constant on the sealed
            // `Numeric` trait. Each blessed numeric type names its
            // own Postgres cast target there; see
            // [`Numeric::SUM_CAST`] for the full rationale.
            cast_to: Some(<V as Numeric>::SUM_CAST),
            distinct: false,
            window: None,
        })
    }

    /// `AVG(column)` — returns `f64`.
    ///
    /// Averages non-null values. Postgres returns `NUMERIC` for
    /// integer inputs and `DOUBLE PRECISION` for floating-point
    /// inputs; the typed surface pins `Out = f64` for both by
    /// emitting an explicit `::DOUBLE PRECISION` cast so the decoder
    /// returns uniformly `f64`. Callers who need `Decimal`-precision
    /// averages use `ctx.raw_scalar` until Phase 5's `Decimal` support
    /// lands.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn avg(self) -> AggregateExpr<f64> {
        AggregateExpr::from_node(ExprNode::Aggregate {
            op: AggOp::Avg,
            arg: Box::new(ExprNode::Field {
                column: self.column(),
            }),
            filter: None,
            // Always DOUBLE PRECISION regardless of the input numeric
            // type — the typed surface's `Out = f64` promise holds
            // uniformly.
            cast_to: Some(<V as Numeric>::AVG_CAST),
            distinct: false,
            window: None,
        })
    }
}

// ── MIN / MAX ─────────────────────────────────────────────────────────
//
// Bound on `V: IntoFilterValue` — every SQL-bindable type Djogi ships
// with already implements that trait (see `query::field::IntoFilterValue`).
// Mirroring that bound here keeps the set of MIN/MAX-able columns in
// lockstep with the set of filter-able columns: one seal to extend
// when a future phase adds a new column type (e.g. `Decimal` in
// Phase 5). Rust `Ord` is deliberately not used — `f64` doesn't
// implement it, but Postgres `MIN(col)` / `MAX(col)` on a `DOUBLE
// PRECISION` column is a routine query.

impl<M: Model, V> FieldRef<M, V>
where
    V: crate::query::field::IntoFilterValue,
{
    /// `MIN(column)` — returns `V`.
    ///
    /// Returns the smallest non-null value of the column per
    /// Postgres' per-type ordering. The bound on
    /// [`crate::query::field::IntoFilterValue`] mirrors the set of
    /// column types Djogi supports as filter RHS values; this keeps
    /// MIN/MAX-able columns aligned with filterable columns without
    /// introducing a parallel seal.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn min(self) -> AggregateExpr<V> {
        AggregateExpr::from_node(ExprNode::Aggregate {
            op: AggOp::Min,
            arg: Box::new(ExprNode::Field {
                column: self.column(),
            }),
            filter: None,
            // MIN / MAX return the column's own type — no widening,
            // no cast needed.
            cast_to: None,
            distinct: false,
            window: None,
        })
    }

    /// `MAX(column)` — returns `V`.
    ///
    /// Returns the largest non-null value of the column. Same bound
    /// rationale as [`FieldRef::min`].
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn max(self) -> AggregateExpr<V> {
        AggregateExpr::from_node(ExprNode::Aggregate {
            op: AggOp::Max,
            arg: Box::new(ExprNode::Field {
                column: self.column(),
            }),
            filter: None,
            cast_to: None,
            distinct: false,
            window: None,
        })
    }
}

// ── ARRAY_AGG / JSON_AGG ───────────────────────────────────────────────────
//
// Available on every `FieldRef<M, V>` regardless of `V` — Postgres can
// ARRAY_AGG / JSONB_AGG any column type. The return type:
// - `array_agg()` → `AggregateExpr<Vec<V>>`: the annotate decode path calls
//   `row.try_get::<_, Vec<V>>(alias)`, which postgres-types handles via its
//   built-in array decoding when `V: FromSql`.
// - `json_agg()` → `AggregateExpr<serde_json::Value>`: JSONB_AGG always
//   produces a JSON array; decoding into `serde_json::Value` covers every
//   element type without requiring `V`-specific codec knowledge.

impl<M: Model, V> FieldRef<M, V> {
    /// `ARRAY_AGG(column)` — collects non-null column values into a Postgres
    /// array, returned as `Vec<V>` at the Rust level.
    ///
    /// postgres-types decodes a Postgres array column into `Vec<V>` when `V`
    /// implements `FromSql`; all scalar column types Djogi ships satisfy that
    /// bound. If `V` does not implement `FromSql`, the failure is a runtime
    /// decode error at fetch time, not a compile error here, because
    /// `FieldRef` is constructed at macro-expansion time with a type the
    /// framework knows is decodable.
    ///
    /// The aggregate emits `ARRAY_AGG(column)` without any narrowing cast.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn array_agg(self) -> AggregateExpr<Vec<V>> {
        AggregateExpr::from_node(ExprNode::Aggregate {
            op: AggOp::ArrayAgg,
            arg: Box::new(ExprNode::Field {
                column: self.column(),
            }),
            filter: None,
            cast_to: None,
            distinct: false,
            window: None,
        })
    }

    /// `JSONB_AGG(column)` — aggregates column values into a JSON array,
    /// returned as `serde_json::Value`.
    ///
    /// Djogi standardises on JSONB for all JSON storage and wire formats
    /// (see `docs/spec/decisions.md`), so `JSONB_AGG` is emitted rather
    /// than `JSON_AGG`. The returned `serde_json::Value` is always a
    /// `Value::Array` wrapping the per-row column values; callers can
    /// pattern-match or call `.as_array()` to iterate.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn json_agg(self) -> AggregateExpr<serde_json::Value> {
        AggregateExpr::from_node(ExprNode::Aggregate {
            op: AggOp::JsonAgg,
            arg: Box::new(ExprNode::Field {
                column: self.column(),
            }),
            filter: None,
            cast_to: None,
            distinct: false,
            window: None,
        })
    }
}

// ── STRING_AGG ──────────────────────────────────────────────────────────────
//
// Gated on `V = String` — string concatenation is only meaningful on TEXT
// columns. The separator is user-supplied at call time and bound as a
// parameter (never interpolated into the SQL string) to guard against
// injection from a runtime-computed separator value.

impl<M: Model> FieldRef<M, String> {
    /// `STRING_AGG(column, sep)` — concatenates non-null string values with
    /// a separator, returned as `String`.
    ///
    /// The separator is bound as a positional parameter (`$N`) rather than
    /// interpolated directly into the SQL string, which means even a separator
    /// that contains SQL metacharacters is handled safely by the Postgres wire
    /// protocol.
    ///
    /// # Example
    ///
    /// ```ignore
    /// Post::objects()
    ///     .annotate(|f| f.title().string_agg(", "))
    ///     .fetch_all(&mut ctx).await?
    /// // → Vec<(Post, String)>  where the String is "Post A, Post B, ..."
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn string_agg(self, sep: impl Into<String>) -> AggregateExpr<String> {
        AggregateExpr::from_node(ExprNode::Aggregate {
            op: AggOp::StringAgg(sep.into()),
            arg: Box::new(ExprNode::Field {
                column: self.column(),
            }),
            filter: None,
            cast_to: None,
            distinct: false,
            window: None,
        })
    }
}

// ── BOOL_AND / BOOL_OR ──────────────────────────────────────────────────────
//
// Gated on `V = bool` — boolean aggregates are only meaningful on BOOLEAN
// columns. Postgres emits NULL for an empty set; the typed surface returns
// `bool` which will be a runtime decode error on an empty grouping. Callers
// that need NULL-safe semantics wrap `Out` in `Option<bool>` themselves at
// the call site by using `ctx.raw_scalar` until a typed `Option<V>` decode
// path lands.

impl<M: Model> FieldRef<M, bool> {
    /// `BOOL_AND(column)` — returns `true` if every non-null value in the
    /// column is `true`, `false` if any non-null value is `false`.
    ///
    /// Returns `NULL` (decoded as a runtime error on the non-`Option` return
    /// type) when the grouping has no rows. Callers operating on potentially
    /// empty groups should use `ctx.raw_scalar` with `COALESCE(BOOL_AND(...),
    /// true)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn bool_and(self) -> AggregateExpr<bool> {
        AggregateExpr::from_node(ExprNode::Aggregate {
            op: AggOp::BoolAnd,
            arg: Box::new(ExprNode::Field {
                column: self.column(),
            }),
            filter: None,
            cast_to: None,
            distinct: false,
            window: None,
        })
    }

    /// `BOOL_OR(column)` — returns `true` if at least one non-null value in
    /// the column is `true`, `false` if all non-null values are `false`.
    ///
    /// Same NULL behaviour as [`FieldRef::bool_and`] — empty groups produce
    /// NULL which decodes as a runtime error on the non-`Option` surface.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn bool_or(self) -> AggregateExpr<bool> {
        AggregateExpr::from_node(ExprNode::Aggregate {
            op: AggOp::BoolOr,
            arg: Box::new(ExprNode::Field {
                column: self.column(),
            }),
            filter: None,
            cast_to: None,
            distinct: false,
            window: None,
        })
    }
}

#[cfg(test)]
mod tests {
    //! Emitter unit tests — each aggregate variant produces the
    //! expected SQL token stream with bind slots in the right
    //! positions.

    use super::*;
    use crate::descriptor::ModelDescriptor;
    use crate::expr::sql::emit_expr;
    use crate::pg::accumulator::SqlAccumulator;

    // Inert local model — mirrors the `Fake` stub used across the
    // expr/query unit tests. Only `table_name` matters for these
    // emission tests; no CRUD path runs.
    struct Txn;
    impl crate::model::__sealed::Sealed for Txn {}
    #[allow(clippy::manual_async_fn)]
    impl crate::model::Model for Txn {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "txns"
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
    fn emit_sum_field() {
        // Bare `emit_expr` on an aggregate node emits just the
        // function call — the narrowing cast lives at the terminal
        // layer (see `query::sql::emit_aggregate_with_cast` for the
        // SELECT-scalar path and
        // `query::sql::emit_aggregate_with_window_and_cast` for the
        // annotate-SELECT-list path). This test pins the bare-node
        // emission; terminal-level tests in `query::annotate::tests`
        // cover the wrapped forms.
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = f.sum();
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &agg.node);
        let sql = qb.sql();
        assert_eq!(sql.trim(), "SUM(amount)", "got: {sql}");
    }

    #[test]
    fn emit_count_field() {
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = f.count();
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &agg.node);
        let sql = qb.sql();
        assert_eq!(sql.trim(), "COUNT(amount)", "got: {sql}");
    }

    #[test]
    fn emit_count_star() {
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = f.count_star();
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &agg.node);
        let sql = qb.sql();
        assert_eq!(sql.trim(), "COUNT(*)", "got: {sql}");
    }

    #[test]
    fn emit_avg_field() {
        // Bare node emission — same rationale as `emit_sum_field`.
        // Terminal layer wraps with the narrowing cast.
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = f.avg();
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &agg.node);
        let sql = qb.sql();
        assert_eq!(sql.trim(), "AVG(amount)", "got: {sql}");
    }

    #[test]
    fn emit_min_max_field() {
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &f.min().node);
        assert_eq!(qb.sql().trim(), "MIN(amount)");

        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &f.max().node);
        assert_eq!(qb.sql().trim(), "MAX(amount)");
    }

    #[test]
    fn emit_aggregate_with_filter() {
        // `f.amount.count().filter(f.amount.as_expr().lt(0))` must
        // emit `COUNT(amount) FILTER (WHERE amount < $1)`. One bind
        // for the literal 0 on the RHS; the column refs are bare.
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let g: FieldRef<Txn, i64> = FieldRef::new("amount");
        let cond = f.as_expr().lt(Expr::literal(0i64));
        let agg = g.count().filter(cond);
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &agg.node);
        let sql = qb.sql();
        assert!(
            sql.contains("COUNT(amount) FILTER (WHERE amount < $1)"),
            "got: {sql}"
        );
    }

    #[test]
    fn filter_overwrites_previous_filter() {
        // The second `.filter(..)` call replaces the first — last
        // call wins. Matches the `QuerySet::limit` pattern.
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let g: FieldRef<Txn, i64> = FieldRef::new("amount");
        let a = f.as_expr().lt(Expr::literal(0i64));
        let b = g.as_expr().gt(Expr::literal(100i64));
        let h: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = h.count().filter(a).filter(b);
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &agg.node);
        let sql = qb.sql();
        // Second filter (gt) should appear; first (lt) should not.
        assert!(sql.contains("amount > $1"), "got: {sql}");
        assert!(
            !sql.contains("amount < "),
            "first filter should be gone: {sql}"
        );
    }

    #[test]
    fn emit_array_agg() {
        let f: FieldRef<Txn, String> = FieldRef::new("tag");
        let agg = f.array_agg();
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &agg.node);
        let sql = qb.sql();
        assert_eq!(sql.trim(), "ARRAY_AGG(tag)", "got: {sql}");
    }

    #[test]
    fn emit_json_agg() {
        let f: FieldRef<Txn, String> = FieldRef::new("tag");
        let agg = f.json_agg();
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &agg.node);
        let sql = qb.sql();
        assert_eq!(sql.trim(), "JSONB_AGG(tag)", "got: {sql}");
    }

    #[test]
    fn emit_string_agg_binds_separator() {
        let f: FieldRef<Txn, String> = FieldRef::new("tag");
        let agg = f.string_agg(", ");
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &agg.node);
        let sql = qb.sql();
        // Column is bare, separator is a bound parameter ($1).
        assert!(sql.contains("STRING_AGG(tag, $1)"), "got: {sql}");
    }

    #[test]
    fn emit_bool_and() {
        let f: FieldRef<Txn, bool> = FieldRef::new("active");
        let agg = f.bool_and();
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &agg.node);
        let sql = qb.sql();
        assert_eq!(sql.trim(), "BOOL_AND(active)", "got: {sql}");
    }

    #[test]
    fn emit_bool_or() {
        let f: FieldRef<Txn, bool> = FieldRef::new("active");
        let agg = f.bool_or();
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &agg.node);
        let sql = qb.sql();
        assert_eq!(sql.trim(), "BOOL_OR(active)", "got: {sql}");
    }
}
