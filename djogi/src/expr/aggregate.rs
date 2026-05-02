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
use crate::expr::window::WindowBuilder;
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

    /// Build an `AggregateExpr<Out>` for the unary `AGG(column)` shape.
    ///
    /// Eleven typed builders on `FieldRef` (`count`, `count_star`, `sum`,
    /// `avg`, `min`, `max`, `array_agg`, `json_agg`, `bool_and`, `bool_or`)
    /// constructed the same six-field `ExprNode::Aggregate { ... }`
    /// literal that varied only in `op`, `column`, and `cast_to`. This
    /// helper consolidates the construction; `string_agg` keeps its own
    /// path because it carries a separator.
    pub(crate) fn unary_agg(
        op: AggOp,
        column: &'static str,
        cast_to: Option<&'static str>,
    ) -> Self {
        AggregateExpr::from_node(ExprNode::Aggregate {
            op,
            arg: Box::new(ExprNode::Field { column }),
            arg2: None,
            filter: None,
            cast_to,
            distinct: false,
            window: None,
            order_by: Vec::new(),
        })
    }

    /// Build an `AggregateExpr<Out>` for the binary `AGG(y, x)` shape.
    ///
    /// Powers the two-arg aggregate family — `COVAR_POP`, `COVAR_SAMP`,
    /// `CORR`, the `REGR_*` regression family (T6), and the JSON-object
    /// aggregates (T9 — `JSON_OBJECT_AGG` / `JSONB_OBJECT_AGG`). Layout
    /// mirrors [`Self::unary_agg`] but populates the `arg2` slot with
    /// the second column reference.
    ///
    /// Argument convention: for the stats / regression family, `y_column`
    /// is the dependent variable (first), `x_column` is the independent
    /// variable (second). For JSON-object aggregates, `y_column` is the
    /// key and `x_column` is the value.
    pub(crate) fn binary_agg(
        op: AggOp,
        y_column: &'static str,
        x_column: &'static str,
        cast_to: Option<&'static str>,
    ) -> Self {
        AggregateExpr::from_node(ExprNode::Aggregate {
            op,
            arg: Box::new(ExprNode::Field { column: y_column }),
            arg2: Some(Box::new(ExprNode::Field { column: x_column })),
            filter: None,
            cast_to,
            distinct: false,
            window: None,
            order_by: Vec::new(),
        })
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

    /// Apply the `DISTINCT` modifier to this aggregate, emitting
    /// `AGG(DISTINCT col)` rather than `AGG(col)`.
    ///
    /// # Accepted aggregates
    ///
    /// Valid on COUNT, SUM, AVG, MIN, MAX, ARRAY_AGG, JSONB_AGG,
    /// BOOL_AND, BOOL_OR (though BOOL_AND/BOOL_OR with DISTINCT is
    /// semantically a no-op — Postgres accepts it, and we emit it
    /// verbatim).
    ///
    /// # Rejected at fetch time
    ///
    /// Two combinations are detected at fetch time and surface as
    /// [`crate::DjogiError::UnsupportedAggregate`]:
    ///
    /// - `COUNT(*)` with `DISTINCT`: `COUNT(DISTINCT *)` is not valid SQL.
    ///   Use `COUNT(DISTINCT col)` via [`FieldRef::count`] instead.
    /// - `STRING_AGG(DISTINCT col, sep)`: Postgres requires an explicit
    ///   per-aggregate `ORDER BY` clause when DISTINCT is used with
    ///   `STRING_AGG`, and Djogi's Phase 6.5 IR does not track per-aggregate
    ///   ORDER BY. This combination will be rejected at fetch time rather
    ///   than producing invalid SQL.
    ///
    /// # Overwrite semantics
    ///
    /// Calling `.distinct()` on an already-distinct aggregate is a no-op —
    /// the flag is already set. This matches the [`QuerySet::limit`] pattern
    /// where the last call wins.
    #[must_use = "AggregateExpr is a value — dropping discards the DISTINCT flag"]
    pub fn distinct(mut self) -> Self {
        if let ExprNode::Aggregate { distinct, .. } = &mut self.node {
            *distinct = true;
        }
        self
    }

    /// Promote this aggregate to a windowed aggregate via a [`WindowBuilder`].
    ///
    /// The builder is passed as a closure that receives a fresh
    /// `WindowBuilder` and returns the configured one:
    ///
    /// ```ignore
    /// // Empty window — identical to the `annotate` default.
    /// f.amount().sum().over(|w| w)
    ///
    /// // Partitioned + ordered window.
    /// f.amount().sum().over(|w| {
    ///     w.partition_by(f.org_id())
    ///      .order_by(f.created_at())
    /// })
    ///
    /// // Rolling 3-row average.
    /// f.score().avg().over(|w| {
    ///     w.order_by(f.created_at())
    ///      .rows(FrameBound::Preceding(3), FrameBound::CurrentRow)
    /// })
    /// ```
    ///
    /// # Overwrite semantics
    ///
    /// Calling `.over(...)` twice replaces the previous window spec — the
    /// last call wins, matching the `QuerySet::limit` pattern.
    ///
    /// # Interaction with `.filter(...)`
    ///
    /// `.over(...)` and `.filter(...)` compose: the `FILTER (WHERE ...)` clause
    /// is emitted before the `OVER (...)` clause, which is the correct Postgres
    /// syntax. Chain order does not matter — both are stored independently on
    /// the node.
    #[must_use = "AggregateExpr is a value — dropping discards the window spec"]
    pub fn over<F>(mut self, f: F) -> Self
    where
        F: FnOnce(WindowBuilder) -> WindowBuilder,
    {
        if let ExprNode::Aggregate { window, .. } = &mut self.node {
            *window = Some(f(WindowBuilder::new()).build());
        }
        self
    }

    /// Append a per-aggregate `ORDER BY` key:
    /// `AGG(arg ORDER BY <ord1>, <ord2>, ...)`.
    ///
    /// Each call appends to the ordering list, matching the
    /// [`crate::query::QuerySet::order_by`] convention. For multiple
    /// keys, chain. The list is emitted inside the aggregate's parens —
    /// for `STRING_AGG` it lands after the separator; for every other
    /// aggregate it lands immediately after the argument.
    ///
    /// # Why this matters
    ///
    /// Several aggregates' result depends on input order: `ARRAY_AGG`,
    /// `JSONB_AGG`, `STRING_AGG`. Without per-aggregate `ORDER BY`,
    /// callers can't get a deterministic result without first wrapping
    /// the entire query in a derived table (much less ergonomic, and
    /// disables grouped-aggregate composition).
    ///
    /// # Special-case unblocked: `STRING_AGG(DISTINCT col, sep)`
    ///
    /// Postgres requires a per-aggregate `ORDER BY` whenever `DISTINCT`
    /// is used with `STRING_AGG`. Without this method, the combination
    /// was rejected at fetch time with
    /// [`crate::DjogiError::UnsupportedAggregate`]. With it, the
    /// well-formed shape
    /// `string_agg(", ").distinct().order_by(f.rank().asc())` becomes
    /// valid; the legality check still rejects `STRING_AGG(DISTINCT ...)`
    /// when the ORDER BY list is empty.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // ARRAY_AGG(id ORDER BY id ASC) — deterministic id list per group
    /// f.id().array_agg().order_by(f.id().asc())
    ///
    /// // STRING_AGG(name, ', ' ORDER BY rank DESC, name ASC) — multi-key
    /// f.name().string_agg(", ")
    ///     .order_by(f.rank().desc())
    ///     .order_by(f.name().asc())
    ///
    /// // ARRAY_AGG(DISTINCT tag ORDER BY tag) — distinct + ordered
    /// f.tag().array_agg().distinct().order_by(f.tag().asc())
    /// ```
    #[must_use = "AggregateExpr is a value — dropping discards the ORDER BY"]
    pub fn order_by(mut self, ord: crate::query::order::OrderExpr) -> Self {
        if let ExprNode::Aggregate { order_by, .. } = &mut self.node {
            order_by.push(ord);
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
        AggregateExpr::unary_agg(AggOp::Count, self.column(), None)
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
        // `arg` is a placeholder for layout uniformity — the emitter
        // renders `COUNT(*)` and ignores the field slot on the
        // CountStar branch.
        AggregateExpr::unary_agg(AggOp::CountStar, self.column(), None)
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
        // `V::SUM_CAST` — associated constant on the sealed `Numeric`
        // trait. Each blessed numeric type names its own Postgres cast
        // target there.
        AggregateExpr::unary_agg(AggOp::Sum, self.column(), Some(<V as Numeric>::SUM_CAST))
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
        // Always DOUBLE PRECISION regardless of the input numeric type
        // — the typed surface's `Out = f64` promise holds uniformly.
        AggregateExpr::unary_agg(AggOp::Avg, self.column(), Some(<V as Numeric>::AVG_CAST))
    }

    /// `STDDEV_POP(column)` — population standard deviation, returned as
    /// `f64`.
    ///
    /// Postgres returns `NUMERIC` for integer inputs and `DOUBLE PRECISION`
    /// for floating-point inputs; the explicit `::DOUBLE PRECISION` cast
    /// narrows uniformly to `f64` so the typed surface's `Out = f64`
    /// promise holds across all blessed numeric column types. Use this
    /// when you have data for the entire population and want the exact
    /// dispersion measure (no sample-correction `n-1` term).
    ///
    /// # Empty / single-row groups
    ///
    /// Returns `NULL` when the group has zero non-null rows; with only
    /// one non-null row, the population stddev is `0`. The non-`Option`
    /// return type means callers operating on potentially empty groups
    /// should use `ctx.raw_scalar` with `COALESCE(STDDEV_POP(...), 0)`
    /// or wrap the column type in `Option<f64>` once that decode path
    /// lands.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn stddev_pop(self) -> AggregateExpr<f64> {
        AggregateExpr::unary_agg(AggOp::StddevPop, self.column(), Some("DOUBLE PRECISION"))
    }

    /// `STDDEV_SAMP(column)` — sample standard deviation, returned as `f64`.
    ///
    /// Uses the Bessel-corrected formula (divides by `n-1`), the standard
    /// choice when treating the rows as a sample of a larger population.
    /// Empty groups and single-row groups return `NULL` (Postgres divides
    /// by zero in the latter case).
    ///
    /// See [`FieldRef::stddev`] for the alias spelling and
    /// [`FieldRef::stddev_pop`] for the population form.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn stddev_samp(self) -> AggregateExpr<f64> {
        AggregateExpr::unary_agg(AggOp::StddevSamp, self.column(), Some("DOUBLE PRECISION"))
    }

    /// `STDDEV(column)` — Postgres alias for [`FieldRef::stddev_samp`].
    /// Both produce identical results; the emitter preserves the spelling
    /// the caller used (matching the [`FieldRef::every`] alias treatment).
    ///
    /// Adopters reading SQL-standard docs reach for `STDDEV_SAMP`;
    /// adopters reading `pg` docs typically write `STDDEV`. Both
    /// work, both round-trip exactly as written.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn stddev(self) -> AggregateExpr<f64> {
        AggregateExpr::unary_agg(AggOp::Stddev, self.column(), Some("DOUBLE PRECISION"))
    }

    /// `VAR_POP(column)` — population variance, returned as `f64`.
    ///
    /// Population form (no `n-1` correction). Same NULL-on-empty-group
    /// behaviour as the stddev pair; same `DOUBLE PRECISION` narrowing
    /// cast applies.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn var_pop(self) -> AggregateExpr<f64> {
        AggregateExpr::unary_agg(AggOp::VarPop, self.column(), Some("DOUBLE PRECISION"))
    }

    /// `VAR_SAMP(column)` — sample variance, returned as `f64`.
    ///
    /// Bessel-corrected (`n-1`) form. Returns `NULL` for empty groups
    /// and for single-row groups (division by zero on `n-1`).
    ///
    /// See [`FieldRef::variance`] for the alias spelling and
    /// [`FieldRef::var_pop`] for the population form.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn var_samp(self) -> AggregateExpr<f64> {
        AggregateExpr::unary_agg(AggOp::VarSamp, self.column(), Some("DOUBLE PRECISION"))
    }

    /// `VARIANCE(column)` — Postgres alias for [`FieldRef::var_samp`].
    /// Same spelling-preservation contract as [`FieldRef::stddev`] /
    /// [`FieldRef::every`].
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn variance(self) -> AggregateExpr<f64> {
        AggregateExpr::unary_agg(AggOp::Variance, self.column(), Some("DOUBLE PRECISION"))
    }

    /// `COVAR_POP(y, x)` — population covariance, returned as `f64`.
    ///
    /// Self is `y` (dependent variable); the argument is `x`
    /// (independent variable). This matches Postgres' convention across
    /// the regression / covariance family — the `y` column is always
    /// the first argument.
    ///
    /// Cast to `DOUBLE PRECISION` so the typed surface's `Out = f64`
    /// promise holds for any combination of integer / float column
    /// types on either side. Both sides gate on the sealed `Numeric`
    /// trait — `covar_pop` between a `String` column and an `i64`
    /// column is a compile error, not a runtime Postgres type error.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Population covariance of order_total vs cost
    /// let cov: f64 = Order::objects()
    ///     .aggregate(|f| f.order_total().covar_pop(f.cost()))
    ///     .fetch_one(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn covar_pop<V2: Numeric>(self, x: FieldRef<M, V2>) -> AggregateExpr<f64> {
        AggregateExpr::binary_agg(
            AggOp::CovarPop,
            self.column(),
            x.column(),
            Some("DOUBLE PRECISION"),
        )
    }

    /// `COVAR_SAMP(y, x)` — sample covariance, returned as `f64`.
    ///
    /// Bessel-corrected (`n-1`) form. Same `y, x` argument convention
    /// and `DOUBLE PRECISION` cast as [`FieldRef::covar_pop`].
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn covar_samp<V2: Numeric>(self, x: FieldRef<M, V2>) -> AggregateExpr<f64> {
        AggregateExpr::binary_agg(
            AggOp::CovarSamp,
            self.column(),
            x.column(),
            Some("DOUBLE PRECISION"),
        )
    }

    /// `CORR(y, x)` — Pearson correlation coefficient, returned as `f64`.
    ///
    /// Result range is `[-1.0, 1.0]`: `1.0` for perfect positive linear
    /// correlation, `-1.0` for perfect negative, `0.0` for no linear
    /// relationship. Same `y, x` argument convention and
    /// `DOUBLE PRECISION` cast as [`FieldRef::covar_pop`].
    ///
    /// # Empty / single-row groups
    ///
    /// Returns `NULL` for empty groups and for groups where one of the
    /// columns has zero variance (Postgres divides by the product of
    /// the two stddevs); the non-`Option` return type means callers
    /// operating on potentially degenerate groups should use
    /// `ctx.raw_scalar` with `COALESCE(CORR(...), 0)`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn corr<V2: Numeric>(self, x: FieldRef<M, V2>) -> AggregateExpr<f64> {
        AggregateExpr::binary_agg(
            AggOp::Corr,
            self.column(),
            x.column(),
            Some("DOUBLE PRECISION"),
        )
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
        // MIN / MAX return the column's own type — no widening, no cast needed.
        AggregateExpr::unary_agg(AggOp::Min, self.column(), None)
    }

    /// `MAX(column)` — returns `V`.
    ///
    /// Returns the largest non-null value of the column. Same bound
    /// rationale as [`FieldRef::min`].
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn max(self) -> AggregateExpr<V> {
        AggregateExpr::unary_agg(AggOp::Max, self.column(), None)
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
        AggregateExpr::unary_agg(AggOp::ArrayAgg, self.column(), None)
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
        AggregateExpr::unary_agg(AggOp::JsonAgg, self.column(), None)
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
        // StringAgg carries a separator, so it doesn't fit `unary_agg`.
        AggregateExpr::from_node(ExprNode::Aggregate {
            op: AggOp::StringAgg(sep.into()),
            arg: Box::new(ExprNode::Field {
                column: self.column(),
            }),
            arg2: None,
            filter: None,
            cast_to: None,
            distinct: false,
            window: None,
            order_by: Vec::new(),
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
        AggregateExpr::unary_agg(AggOp::BoolAnd, self.column(), None)
    }

    /// `BOOL_OR(column)` — returns `true` if at least one non-null value in
    /// the column is `true`, `false` if all non-null values are `false`.
    ///
    /// Same NULL behaviour as [`FieldRef::bool_and`] — empty groups produce
    /// NULL which decodes as a runtime error on the non-`Option` surface.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn bool_or(self) -> AggregateExpr<bool> {
        AggregateExpr::unary_agg(AggOp::BoolOr, self.column(), None)
    }

    /// `EVERY(column)` — Postgres-standard alias for [`FieldRef::bool_and`].
    /// Returns `true` if every non-null value in the column is `true`,
    /// `false` if any non-null value is `false`.
    ///
    /// `EVERY` and `BOOL_AND` are interchangeable in Postgres — they
    /// produce identical results. Djogi exposes both because adopters
    /// reading from one set of docs expect the spelling they know;
    /// the emitter honours the user's choice (a call to `.every()`
    /// emits `EVERY(col)`, not `BOOL_AND(col)`). Same NULL behaviour as
    /// [`FieldRef::bool_and`].
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn every(self) -> AggregateExpr<bool> {
        AggregateExpr::unary_agg(AggOp::Every, self.column(), None)
    }
}

// ── BIT_AND / BIT_OR / BIT_XOR ──────────────────────────────────────────────
//
// Bitwise integer aggregates. Sealed on a separate `IntegerColumn` trait
// rather than `Numeric` because Postgres BIT_AND / BIT_OR / BIT_XOR are
// defined for SMALLINT / INTEGER / BIGINT (and bit-string types Djogi
// doesn't model today) but NOT for floating-point types — `BIT_AND(REAL)`
// is a Postgres type error. Gating on `IntegerColumn` produces a compile-
// time error if a caller writes `f.score().bit_or()` on an `f64` column,
// which beats the runtime Postgres error.
//
// The return type is `Out = V` (the column's own integer type) — Postgres
// returns the same width it operates on, no widening, so no narrowing
// cast is required. Mirrors `min` / `max` in that respect.

mod integer_column_seal {
    /// Local seal for [`super::IntegerColumn`]. Crate-private — downstream
    /// code cannot name `Sealed`, so `impl IntegerColumn for MyType {}`
    /// fails at "the trait `Sealed` is not implemented for `MyType`".
    pub trait Sealed {}

    impl Sealed for i16 {}
    impl Sealed for i32 {}
    impl Sealed for i64 {}
}

/// Sealed marker trait for column types that admit Postgres bitwise
/// aggregate functions ([`FieldRef::bit_and`], [`FieldRef::bit_or`],
/// [`FieldRef::bit_xor`]).
///
/// Implemented for `i16`, `i32`, `i64` — the three integer scalar types
/// Djogi blesses. Floating-point types (`f32`, `f64`), `time::Duration`,
/// and `Decimal` deliberately do NOT implement this; the corresponding
/// `BIT_AND(REAL)` / `BIT_OR(NUMERIC)` calls are Postgres type errors,
/// and gating at the type system catches them at compile time.
///
/// # Why a separate trait from [`super::arithmetic::Numeric`]?
///
/// `Numeric` admits floats and `Duration` for arithmetic operator
/// composition. Bit aggregates are integer-only — a separate trait
/// keeps the gating precise. The two traits overlap on `i16`/`i32`/`i64`;
/// any column type qualifies for both arithmetic and bit aggregates iff
/// it implements both.
pub trait IntegerColumn: integer_column_seal::Sealed {}

impl IntegerColumn for i16 {}
impl IntegerColumn for i32 {}
impl IntegerColumn for i64 {}

impl<M: Model, V: IntegerColumn> FieldRef<M, V> {
    /// `BIT_AND(column)` — bitwise AND across non-null integer values,
    /// returned as the column's integer type.
    ///
    /// Useful for flag-bitmask reduction: `f.flags().bit_and()` returns
    /// the set of bits set in *every* non-null row of the group.
    /// Identity (no rows or all NULL): all-bits-set in two's complement
    /// (`-1` for signed types). Empty groups decode as NULL — wrap `Out`
    /// in `Option<V>` at the call site (or use a `FILTER (WHERE ...)`
    /// guard) for NULL-safe handling.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Bits common to every flag value across the org's users
    /// let common_bits: i32 = User::objects()
    ///     .filter(|u| u.org_id().eq(my_org))
    ///     .aggregate(|u| u.flags().bit_and())
    ///     .fetch_one(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn bit_and(self) -> AggregateExpr<V> {
        AggregateExpr::unary_agg(AggOp::BitAnd, self.column(), None)
    }

    /// `BIT_OR(column)` — bitwise OR across non-null integer values.
    ///
    /// Useful for "did any row have flag X set?" reductions:
    /// `f.flags().bit_or()` returns the union of bits across the group.
    /// Identity: 0 (no bits set).
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn bit_or(self) -> AggregateExpr<V> {
        AggregateExpr::unary_agg(AggOp::BitOr, self.column(), None)
    }

    /// `BIT_XOR(column)` — bitwise XOR across non-null integer values.
    ///
    /// Useful for parity / checksum-style aggregations. Postgres 14+
    /// adds this aggregate; Djogi's PostgreSQL 18 floor includes it.
    /// Identity: 0.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn bit_xor(self) -> AggregateExpr<V> {
        AggregateExpr::unary_agg(AggOp::BitXor, self.column(), None)
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

    // ── .distinct() tests (T4) ────────────────────────────────────────────────

    #[test]
    fn sum_distinct_emits_sum_distinct() {
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = f.sum().distinct();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert!(
            acc.sql().contains("SUM(DISTINCT amount)"),
            "got: {}",
            acc.sql()
        );
    }

    #[test]
    fn count_distinct_emits_count_distinct() {
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = f.count().distinct();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert!(
            acc.sql().contains("COUNT(DISTINCT amount)"),
            "got: {}",
            acc.sql()
        );
    }

    #[test]
    fn avg_distinct_emits_avg_distinct() {
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = f.avg().distinct();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert!(
            acc.sql().contains("AVG(DISTINCT amount)"),
            "got: {}",
            acc.sql()
        );
    }

    #[test]
    fn min_distinct_emits_min_distinct() {
        // MIN(DISTINCT col) is valid Postgres syntax — emits as-is.
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = f.min().distinct();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert!(
            acc.sql().contains("MIN(DISTINCT amount)"),
            "got: {}",
            acc.sql()
        );
    }

    #[test]
    fn max_distinct_emits_max_distinct() {
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = f.max().distinct();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert!(
            acc.sql().contains("MAX(DISTINCT amount)"),
            "got: {}",
            acc.sql()
        );
    }

    #[test]
    fn array_agg_distinct_emits_array_agg_distinct() {
        let f: FieldRef<Txn, String> = FieldRef::new("tag");
        let agg = f.array_agg().distinct();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert!(
            acc.sql().contains("ARRAY_AGG(DISTINCT tag)"),
            "got: {}",
            acc.sql()
        );
    }

    #[test]
    fn json_agg_distinct_emits_jsonb_agg_distinct() {
        let f: FieldRef<Txn, String> = FieldRef::new("tag");
        let agg = f.json_agg().distinct();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert!(
            acc.sql().contains("JSONB_AGG(DISTINCT tag)"),
            "got: {}",
            acc.sql()
        );
    }

    #[test]
    fn bool_and_distinct_emits_bool_and_distinct() {
        // BOOL_AND(DISTINCT col) is valid Postgres syntax — effectively a
        // no-op semantically (distinctness doesn't change a boolean AND) but
        // Postgres accepts it. We emit it as-is.
        let f: FieldRef<Txn, bool> = FieldRef::new("active");
        let agg = f.bool_and().distinct();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert!(
            acc.sql().contains("BOOL_AND(DISTINCT active)"),
            "got: {}",
            acc.sql()
        );
    }

    #[test]
    fn bool_or_distinct_emits_bool_or_distinct() {
        let f: FieldRef<Txn, bool> = FieldRef::new("active");
        let agg = f.bool_or().distinct();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert!(
            acc.sql().contains("BOOL_OR(DISTINCT active)"),
            "got: {}",
            acc.sql()
        );
    }

    #[test]
    fn count_star_distinct_rejected_at_fetch() {
        // COUNT(DISTINCT *) is not valid SQL — the distinct flag on a
        // CountStar aggregate must be caught and returned as
        // DjogiError::UnsupportedAggregate before any SQL is emitted.
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let mut agg = f.count_star();
        // Forcibly set distinct on the CountStar node, since the `.distinct()`
        // builder correctly prevents setting distinct on CountStar at the API
        // level. We reach into the node directly (crate-private) to simulate
        // a malformed aggregate that must be caught at fetch time.
        if let ExprNode::Aggregate {
            ref mut distinct, ..
        } = agg.node
        {
            *distinct = true;
        }
        let result = crate::expr::sql::check_aggregate_legality(&agg.node);
        assert!(
            result.is_err(),
            "expected UnsupportedAggregate error for COUNT(DISTINCT *)"
        );
        let err = result.unwrap_err();
        assert!(
            matches!(err, crate::DjogiError::UnsupportedAggregate { .. }),
            "expected UnsupportedAggregate variant, got: {err:?}"
        );
    }

    #[test]
    fn string_agg_distinct_without_order_by_rejected_at_fetch() {
        // STRING_AGG(DISTINCT col, sep) without a per-aggregate ORDER BY is
        // ill-formed Postgres. With Cluster E T1 the IR tracks per-aggregate
        // ORDER BY, so the rejection is scoped to the still-ill-formed
        // no-ORDER-BY case. With ORDER BY chained, the combination is
        // accepted (covered by `string_agg_distinct_with_order_by_is_now_accepted`).
        let f: FieldRef<Txn, String> = FieldRef::new("tag");
        let mut agg = f.string_agg(", ");
        if let ExprNode::Aggregate {
            ref mut distinct, ..
        } = agg.node
        {
            *distinct = true;
        }
        let result = crate::expr::sql::check_aggregate_legality(&agg.node);
        assert!(
            result.is_err(),
            "expected UnsupportedAggregate error for STRING_AGG(DISTINCT ...) with no ORDER BY"
        );
        let err = result.unwrap_err();
        assert!(
            matches!(err, crate::DjogiError::UnsupportedAggregate { .. }),
            "expected UnsupportedAggregate variant, got: {err:?}"
        );
    }

    // ── .order_by(...) per-aggregate ORDER BY tests (T1) ─────────────────────

    #[test]
    fn order_by_appends_to_aggregate_node() {
        // `.order_by(...)` mutates the inner Aggregate node's order_by Vec.
        let f: FieldRef<Txn, i64> = FieldRef::new("id");
        let agg = f.array_agg().order_by(f.asc());
        if let ExprNode::Aggregate { order_by, .. } = &agg.node {
            assert_eq!(
                order_by.len(),
                1,
                "single .order_by call should append exactly one OrderExpr"
            );
        } else {
            panic!("AggregateExpr did not wrap an Aggregate node");
        }
    }

    #[test]
    fn multiple_order_by_calls_append_keys() {
        // Chained `.order_by(...)` calls each append, mirroring
        // QuerySet::order_by semantics.
        let f: FieldRef<Txn, i64> = FieldRef::new("id");
        let g: FieldRef<Txn, i64> = FieldRef::new("rank");
        let agg = f.array_agg().order_by(g.desc()).order_by(f.asc());
        if let ExprNode::Aggregate { order_by, .. } = &agg.node {
            assert_eq!(
                order_by.len(),
                2,
                "chained .order_by calls should append in order"
            );
        } else {
            panic!("AggregateExpr did not wrap an Aggregate node");
        }
    }

    #[test]
    fn array_agg_with_order_by_emits_clause() {
        // Bare emission shape: ARRAY_AGG(id ORDER BY id ASC).
        let f: FieldRef<Txn, i64> = FieldRef::new("id");
        let agg = f.array_agg().order_by(f.asc());
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        let sql = acc.sql().to_string();
        assert!(
            sql.contains("ARRAY_AGG(id ORDER BY id ASC"),
            "expected ARRAY_AGG with ORDER BY clause, got: {sql}"
        );
        assert!(sql.ends_with(')'), "aggregate must close its parens: {sql}");
    }

    #[test]
    fn array_agg_distinct_with_order_by_emits_distinct_and_order_by() {
        // DISTINCT + ORDER BY composes — `ARRAY_AGG(DISTINCT id ORDER BY id ASC)`.
        let f: FieldRef<Txn, i64> = FieldRef::new("id");
        let agg = f.array_agg().distinct().order_by(f.asc());
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        let sql = acc.sql().to_string();
        assert!(
            sql.contains("ARRAY_AGG(DISTINCT id ORDER BY id ASC"),
            "expected ARRAY_AGG(DISTINCT ... ORDER BY ...), got: {sql}"
        );
    }

    #[test]
    fn multiple_order_by_keys_emit_comma_separated() {
        // Two-key ORDER BY: `ARRAY_AGG(id ORDER BY rank DESC, id ASC)`.
        let f: FieldRef<Txn, i64> = FieldRef::new("id");
        let g: FieldRef<Txn, i64> = FieldRef::new("rank");
        let agg = f.array_agg().order_by(g.desc()).order_by(f.asc());
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        let sql = acc.sql().to_string();
        assert!(
            sql.contains("ARRAY_AGG(id ORDER BY rank DESC, id ASC"),
            "expected multi-key ORDER BY with comma separator, got: {sql}"
        );
    }

    #[test]
    fn string_agg_with_order_by_emits_after_separator() {
        // For STRING_AGG, ORDER BY lands AFTER the separator bind, still
        // inside the aggregate parens: `STRING_AGG(name, $1 ORDER BY rank ASC)`.
        let f: FieldRef<Txn, String> = FieldRef::new("name");
        let g: FieldRef<Txn, i64> = FieldRef::new("rank");
        let agg = f.string_agg(", ").order_by(g.asc());
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        let sql = acc.sql().to_string();
        // Bind index is the first available, which depends on accumulator state;
        // assert the structural shape rather than the exact $N.
        assert!(
            sql.starts_with("STRING_AGG(name, $") && sql.contains(" ORDER BY rank ASC"),
            "expected STRING_AGG(col, $sep ORDER BY ...), got: {sql}"
        );
    }

    #[test]
    fn string_agg_distinct_with_order_by_is_now_accepted() {
        // `STRING_AGG(DISTINCT col, sep ORDER BY other)` is well-formed
        // Postgres — the legality check now accepts the combination when
        // an ORDER BY is present (Cluster E T1).
        let f: FieldRef<Txn, String> = FieldRef::new("tag");
        let g: FieldRef<Txn, i64> = FieldRef::new("rank");
        let agg = f.string_agg(", ").distinct().order_by(g.asc());
        let result = crate::expr::sql::check_aggregate_legality(&agg.node);
        assert!(
            result.is_ok(),
            "STRING_AGG(DISTINCT ... ORDER BY ...) should be accepted, got: {result:?}"
        );
    }

    #[test]
    fn string_agg_distinct_with_order_by_emits_correct_sql() {
        // End-to-end: the previously-unsupported combination now emits
        // well-formed SQL: STRING_AGG(DISTINCT name, $1 ORDER BY rank ASC).
        let f: FieldRef<Txn, String> = FieldRef::new("name");
        let g: FieldRef<Txn, i64> = FieldRef::new("rank");
        let agg = f.string_agg(", ").distinct().order_by(g.asc());
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        let sql = acc.sql().to_string();
        assert!(
            sql.starts_with("STRING_AGG(DISTINCT name, $") && sql.contains(" ORDER BY rank ASC"),
            "expected STRING_AGG(DISTINCT col, $sep ORDER BY ...), got: {sql}"
        );
    }

    #[test]
    fn order_by_with_filter_and_distinct_compose() {
        // All three modifiers compose. Emission ordering inside the parens:
        // DISTINCT → arg → ORDER BY. FILTER attaches after the close paren.
        let f: FieldRef<Txn, i64> = FieldRef::new("id");
        let g: FieldRef<Txn, i64> = FieldRef::new("rank");
        let agg = f
            .array_agg()
            .distinct()
            .filter(g.as_expr().gt(Expr::literal(0i64)))
            .order_by(f.asc());
        let mut acc = SqlAccumulator::new("");
        crate::expr::sql::emit_expr(&mut acc, &agg.node);
        let sql = acc.sql().to_string();
        assert!(
            sql.contains("ARRAY_AGG(DISTINCT id ORDER BY id ASC")
                && sql.contains(") FILTER (WHERE rank > "),
            "expected DISTINCT + ORDER BY + FILTER composition, got: {sql}"
        );
    }

    // ── .over(|w| ...) end-to-end tests ──────────────────────────────────────
    //
    // These tests exercise the round-trip: `.over(|w| ...)` on `AggregateExpr`
    // stores a `WindowSpec` on the node, then `emit_aggregate_with_window_and_cast`
    // picks it up and emits the correct `OVER (...)` clause. The bare
    // `emit_expr` path (used for nested aggregates) does NOT emit the window
    // clause — window emission is handled exclusively at the terminal layer.

    #[test]
    fn over_empty_closure_stores_window_spec() {
        // `.over(|w| w)` sets `window: Some(WindowSpec::default())` — the
        // terminal layer will emit `OVER ()` from it, preserving the pre-T3
        // behaviour.
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = f.sum().over(|w| w);
        if let ExprNode::Aggregate { window, .. } = &agg.node {
            assert!(
                window.is_some(),
                "over(|w| w) should set window to Some(..)"
            );
        } else {
            panic!("AggregateExpr did not wrap an Aggregate node");
        }
    }

    #[test]
    fn over_empty_closure_emits_over_parens_via_terminal() {
        // End-to-end: `.over(|w| w)` → `emit_aggregate_with_window_and_cast` →
        // `SUM(amount) OVER ()`. The narrowing cast (SUM_CAST) wraps the whole
        // expression in parens: `(SUM(amount) OVER ())::BIGINT`.
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = f.sum().over(|w| w);
        let mut acc = SqlAccumulator::new("");
        crate::query::sql::emit_aggregate_with_window_and_cast(&mut acc, &agg.node);
        let sql = acc.sql().to_string();
        assert!(
            sql.contains("SUM(amount) OVER ()"),
            "expected OVER () from empty window spec, got: {sql}"
        );
    }

    #[test]
    fn over_with_partition_emits_partition_clause_via_terminal() {
        // `.over(|w| w.partition_by(org_id_ref))` → `OVER (PARTITION BY org_id)`.
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let p: FieldRef<Txn, i64> = FieldRef::new("org_id");
        let agg = f.sum().over(|w| w.partition_by(p));
        let mut acc = SqlAccumulator::new("");
        crate::query::sql::emit_aggregate_with_window_and_cast(&mut acc, &agg.node);
        let sql = acc.sql().to_string();
        assert!(sql.contains("OVER (PARTITION BY org_id)"), "got: {sql}");
    }

    #[test]
    fn over_with_order_by_emits_order_clause_via_terminal() {
        // `.over(|w| w.order_by(created_at_ref))` → `OVER (ORDER BY created_at ASC)`.
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let o: FieldRef<Txn, i64> = FieldRef::new("created_at");
        let agg = f.count().over(|w| w.order_by(o));
        let mut acc = SqlAccumulator::new("");
        crate::query::sql::emit_aggregate_with_window_and_cast(&mut acc, &agg.node);
        let sql = acc.sql().to_string();
        assert!(sql.contains("OVER (ORDER BY created_at ASC)"), "got: {sql}");
    }

    #[test]
    fn over_with_rows_frame_emits_frame_clause_via_terminal() {
        // Rolling 3-row SUM: `OVER (ORDER BY created_at ASC ROWS BETWEEN
        // $1 PRECEDING AND CURRENT ROW)`.
        use crate::expr::window::FrameBound;
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let o: FieldRef<Txn, i64> = FieldRef::new("created_at");
        let agg = f.sum().over(|w| {
            w.order_by(o)
                .rows(FrameBound::Preceding(3), FrameBound::CurrentRow)
        });
        let mut acc = SqlAccumulator::new("");
        crate::query::sql::emit_aggregate_with_window_and_cast(&mut acc, &agg.node);
        let sql = acc.sql().to_string();
        assert!(
            sql.contains("ROWS BETWEEN $1 PRECEDING AND CURRENT ROW"),
            "got: {sql}"
        );
    }

    #[test]
    fn over_replaces_previous_window_spec_last_call_wins() {
        // Calling `.over(...)` twice — the second call replaces the first.
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let p1: FieldRef<Txn, i64> = FieldRef::new("org_id");
        let p2: FieldRef<Txn, i64> = FieldRef::new("dept_id");
        let agg = f
            .sum()
            .over(|w| w.partition_by(p1))
            .over(|w| w.partition_by(p2));
        let mut acc = SqlAccumulator::new("");
        crate::query::sql::emit_aggregate_with_window_and_cast(&mut acc, &agg.node);
        let sql = acc.sql().to_string();
        assert!(sql.contains("PARTITION BY dept_id"), "got: {sql}");
        assert!(!sql.contains("org_id"), "first spec should be gone: {sql}");
    }

    #[test]
    fn no_over_call_preserves_default_over_empty_via_terminal() {
        // When `.over(...)` is never called, `window: None` — the terminal
        // `emit_aggregate_with_window_and_cast` emits `OVER ()` as before T3.
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = f.count();
        let mut acc = SqlAccumulator::new("");
        crate::query::sql::emit_aggregate_with_window_and_cast(&mut acc, &agg.node);
        let sql = acc.sql().to_string();
        assert!(
            sql.contains("COUNT(amount) OVER ()"),
            "default should be OVER (), got: {sql}"
        );
    }

    // ── BIT_AND / BIT_OR / BIT_XOR tests (T2) ────────────────────────────────

    #[test]
    fn bit_and_emits_bit_and_keyword() {
        let f: FieldRef<Txn, i32> = FieldRef::new("flags");
        let agg = f.bit_and();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "BIT_AND(flags)");
    }

    #[test]
    fn bit_or_emits_bit_or_keyword() {
        let f: FieldRef<Txn, i32> = FieldRef::new("flags");
        let agg = f.bit_or();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "BIT_OR(flags)");
    }

    #[test]
    fn bit_xor_emits_bit_xor_keyword() {
        let f: FieldRef<Txn, i64> = FieldRef::new("checksum_part");
        let agg = f.bit_xor();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "BIT_XOR(checksum_part)");
    }

    #[test]
    fn bit_aggregates_compose_with_distinct() {
        // BIT_AND(DISTINCT col) is accepted (Postgres permits it; the
        // result is identical to BIT_AND(col) because DISTINCT just
        // dedupes rows before aggregation, but the emission round-trips).
        let f: FieldRef<Txn, i32> = FieldRef::new("flags");
        let agg = f.bit_and().distinct();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "BIT_AND(DISTINCT flags)");
    }

    #[test]
    fn bit_aggregates_compose_with_filter() {
        // BIT_OR with FILTER attaches the predicate after the close paren.
        let f: FieldRef<Txn, i32> = FieldRef::new("flags");
        let g: FieldRef<Txn, i64> = FieldRef::new("active_at");
        let agg = f.bit_or().filter(g.as_expr().gt(Expr::literal(0i64)));
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        let sql = acc.sql().to_string();
        assert!(
            sql.contains("BIT_OR(flags)") && sql.contains(" FILTER (WHERE active_at > "),
            "expected BIT_OR + FILTER composition, got: {sql}"
        );
    }

    #[test]
    fn bit_aggregates_compose_with_order_by() {
        // BIT aggregates inherit the T1 .order_by modifier — useful for
        // deterministic emission when paired with DISTINCT, even though
        // BIT_AND/OR/XOR are commutative and the result is order-invariant.
        let f: FieldRef<Txn, i32> = FieldRef::new("flags");
        let agg = f.bit_and().distinct().order_by(f.asc());
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "BIT_AND(DISTINCT flags ORDER BY flags ASC)");
    }

    // ── COVAR / CORR (T5 — binary aggregates) ─────────────────────────────

    #[test]
    fn covar_pop_emits_covar_pop_y_x() {
        // Self is y, arg is x. Postgres convention: y first.
        let f_y: FieldRef<Txn, f64> = FieldRef::new("revenue");
        let f_x: FieldRef<Txn, f64> = FieldRef::new("cost");
        let agg = f_y.covar_pop(f_x);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "COVAR_POP(revenue, cost)");
    }

    #[test]
    fn covar_samp_emits_covar_samp_y_x() {
        let f_y: FieldRef<Txn, i64> = FieldRef::new("revenue");
        let f_x: FieldRef<Txn, i64> = FieldRef::new("cost");
        let agg = f_y.covar_samp(f_x);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "COVAR_SAMP(revenue, cost)");
    }

    #[test]
    fn corr_emits_corr_y_x() {
        let f_y: FieldRef<Txn, f64> = FieldRef::new("score_a");
        let f_x: FieldRef<Txn, f64> = FieldRef::new("score_b");
        let agg = f_y.corr(f_x);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "CORR(score_a, score_b)");
    }

    #[test]
    fn covar_pop_composes_with_distinct() {
        // DISTINCT applies to the row tuple — `COVAR_POP(DISTINCT y, x)`
        // is rare but valid Postgres syntax. The emitter places DISTINCT
        // immediately after the open paren, before both args.
        let f_y: FieldRef<Txn, f64> = FieldRef::new("revenue");
        let f_x: FieldRef<Txn, f64> = FieldRef::new("cost");
        let agg = f_y.covar_pop(f_x).distinct();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "COVAR_POP(DISTINCT revenue, cost)");
    }

    #[test]
    fn binary_aggregates_y_first_x_second_argument_order() {
        // Pin the y-then-x convention — swapping the call site (x,y) →
        // (y,x) on COVAR is silently incorrect but Postgres-symmetric;
        // for CORR / regression the order matters and the typed API
        // must thread it through faithfully.
        let f_a: FieldRef<Txn, f64> = FieldRef::new("a_col");
        let f_b: FieldRef<Txn, f64> = FieldRef::new("b_col");
        // a.covar(b) → COVAR(a, b) — self is the first arg.
        let agg_ab = f_a.covar_pop(f_b);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg_ab.node);
        assert_eq!(acc.sql(), "COVAR_POP(a_col, b_col)");

        // b.covar(a) → COVAR(b, a) — receiver order is the SQL order.
        let f_a2: FieldRef<Txn, f64> = FieldRef::new("a_col");
        let f_b2: FieldRef<Txn, f64> = FieldRef::new("b_col");
        let agg_ba = f_b2.covar_pop(f_a2);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg_ba.node);
        assert_eq!(acc.sql(), "COVAR_POP(b_col, a_col)");
    }

    #[test]
    fn binary_aggregates_return_f64() {
        // Compile-time pin: covar_pop / covar_samp / corr all return
        // `AggregateExpr<f64>` regardless of the input numeric types.
        // Mixed-type calls (i32 × f64) compose because both gate on
        // `Numeric` independently.
        let f_y_i64: FieldRef<Txn, i64> = FieldRef::new("y");
        let f_x_i32: FieldRef<Txn, i32> = FieldRef::new("x");
        let f_x_f64: FieldRef<Txn, f64> = FieldRef::new("x_f64");
        let f_x_i64: FieldRef<Txn, i64> = FieldRef::new("x");
        let _: AggregateExpr<f64> = f_y_i64.covar_pop(f_x_i32);
        let _: AggregateExpr<f64> = FieldRef::<Txn, i64>::new("y").covar_samp(f_x_f64);
        let _: AggregateExpr<f64> = FieldRef::<Txn, i64>::new("y").corr(f_x_i64);
    }

    #[test]
    fn unary_aggregates_have_no_arg2_after_t5_infrastructure() {
        // Regression check — adding the `arg2` slot to ExprNode::Aggregate
        // must not affect unary aggregates. Every unary builder
        // (`unary_agg` plus `string_agg`) sets `arg2: None`; the bare
        // unary emission path is untouched.
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = f.sum();
        if let ExprNode::Aggregate { arg2, .. } = &agg.node {
            assert!(
                arg2.is_none(),
                "unary aggregates must leave arg2 empty after the T5 IR change"
            );
        } else {
            panic!("AggregateExpr did not wrap an Aggregate node");
        }
        // STRING_AGG also leaves arg2 empty — separator carries through
        // the AggOp variant payload, not the arg2 slot.
        let f_s: FieldRef<Txn, String> = FieldRef::new("tag");
        let agg_s = f_s.string_agg(", ");
        if let ExprNode::Aggregate { arg2, .. } = &agg_s.node {
            assert!(
                arg2.is_none(),
                "STRING_AGG carries its separator inline; arg2 must stay empty"
            );
        } else {
            panic!("AggregateExpr did not wrap an Aggregate node");
        }
    }

    // ── STDDEV / VARIANCE family (T4) ─────────────────────────────────────

    #[test]
    fn stddev_pop_emits_stddev_pop() {
        let f: FieldRef<Txn, f64> = FieldRef::new("score");
        let agg = f.stddev_pop();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "STDDEV_POP(score)");
    }

    #[test]
    fn stddev_samp_emits_stddev_samp() {
        let f: FieldRef<Txn, f64> = FieldRef::new("score");
        let agg = f.stddev_samp();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "STDDEV_SAMP(score)");
    }

    #[test]
    fn stddev_alias_emits_bare_stddev() {
        // The `.stddev()` alias preserves spelling — emits STDDEV, not
        // STDDEV_SAMP. Both names resolve to the same Postgres aggregate
        // semantically, but the emitter honours the caller's choice.
        let f: FieldRef<Txn, f64> = FieldRef::new("score");
        let agg = f.stddev();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "STDDEV(score)");
    }

    #[test]
    fn var_pop_emits_var_pop() {
        let f: FieldRef<Txn, i64> = FieldRef::new("score");
        let agg = f.var_pop();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "VAR_POP(score)");
    }

    #[test]
    fn var_samp_emits_var_samp() {
        let f: FieldRef<Txn, i64> = FieldRef::new("score");
        let agg = f.var_samp();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "VAR_SAMP(score)");
    }

    #[test]
    fn variance_alias_emits_bare_variance() {
        let f: FieldRef<Txn, i64> = FieldRef::new("score");
        let agg = f.variance();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "VARIANCE(score)");
    }

    #[test]
    fn stats_aggregates_compose_with_distinct() {
        // DISTINCT composes for stats aggregates — emits
        // `STDDEV_POP(DISTINCT score)`. Semantically rare but Postgres
        // accepts it; the round-trip is structural.
        let f: FieldRef<Txn, f64> = FieldRef::new("score");
        let agg = f.stddev_pop().distinct();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "STDDEV_POP(DISTINCT score)");
    }

    #[test]
    fn stddev_alias_pair_emit_distinct_keywords() {
        // `stddev_samp` and `stddev` are *different* AggOp variants that
        // resolve to the same Postgres aggregate semantically. The
        // emitter must keep them distinct in the SQL token stream
        // (no collapse), so the user's spelling round-trips.
        let f1: FieldRef<Txn, f64> = FieldRef::new("score");
        let f2: FieldRef<Txn, f64> = FieldRef::new("score");
        let mut acc1 = SqlAccumulator::new("");
        let mut acc2 = SqlAccumulator::new("");
        emit_expr(&mut acc1, &f1.stddev_samp().node);
        emit_expr(&mut acc2, &f2.stddev().node);
        assert_eq!(acc1.sql(), "STDDEV_SAMP(score)");
        assert_eq!(acc2.sql(), "STDDEV(score)");
        assert_ne!(acc1.sql(), acc2.sql());
    }

    #[test]
    fn variance_alias_pair_emit_distinct_keywords() {
        // Same alias-equivalence pin for VAR_SAMP / VARIANCE.
        let f1: FieldRef<Txn, i64> = FieldRef::new("score");
        let f2: FieldRef<Txn, i64> = FieldRef::new("score");
        let mut acc1 = SqlAccumulator::new("");
        let mut acc2 = SqlAccumulator::new("");
        emit_expr(&mut acc1, &f1.var_samp().node);
        emit_expr(&mut acc2, &f2.variance().node);
        assert_eq!(acc1.sql(), "VAR_SAMP(score)");
        assert_eq!(acc2.sql(), "VARIANCE(score)");
        assert_ne!(acc1.sql(), acc2.sql());
    }

    #[test]
    fn stats_aggregates_return_f64() {
        // Compile-time pin: every stats aggregate returns
        // `AggregateExpr<f64>` regardless of the input numeric type.
        let f_i16: FieldRef<Txn, i16> = FieldRef::new("score16");
        let f_i32: FieldRef<Txn, i32> = FieldRef::new("score32");
        let f_i64: FieldRef<Txn, i64> = FieldRef::new("score64");
        let f_f32: FieldRef<Txn, f32> = FieldRef::new("score_f32");
        let f_f64: FieldRef<Txn, f64> = FieldRef::new("score_f64");
        let _: AggregateExpr<f64> = f_i16.stddev_pop();
        let _: AggregateExpr<f64> = f_i32.stddev_samp();
        let _: AggregateExpr<f64> = f_i64.stddev();
        let _: AggregateExpr<f64> = f_f32.var_pop();
        let _: AggregateExpr<f64> = f_f64.var_samp();
        let _: AggregateExpr<f64> = f_i32.variance();
    }

    // ── EVERY (T3) ────────────────────────────────────────────────────────

    #[test]
    fn every_emits_every_keyword() {
        // The alias preserves spelling — `EVERY(col)`, not `BOOL_AND(col)`.
        let f: FieldRef<Txn, bool> = FieldRef::new("active");
        let agg = f.every();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "EVERY(active)");
    }

    #[test]
    fn every_distinct_emits_every_distinct() {
        // EVERY composes with DISTINCT (semantic no-op on booleans, but
        // accepted by Postgres). The keyword stays `EVERY`.
        let f: FieldRef<Txn, bool> = FieldRef::new("active");
        let agg = f.every().distinct();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node);
        assert_eq!(acc.sql(), "EVERY(DISTINCT active)");
    }

    #[test]
    fn every_returns_bool_aggregate() {
        // Compile-time pin: `every()` returns `AggregateExpr<bool>`,
        // matching `bool_and`'s shape.
        let f: FieldRef<Txn, bool> = FieldRef::new("active");
        let _: AggregateExpr<bool> = f.every();
    }

    #[test]
    fn bit_and_returns_field_value_type() {
        // Compile-time signature check — the return type is
        // AggregateExpr<V> for V matching the column type.
        let f16: FieldRef<Txn, i16> = FieldRef::new("flags16");
        let f32: FieldRef<Txn, i32> = FieldRef::new("flags32");
        let f64: FieldRef<Txn, i64> = FieldRef::new("flags64");
        let _: AggregateExpr<i16> = f16.bit_and();
        let _: AggregateExpr<i32> = f32.bit_or();
        let _: AggregateExpr<i64> = f64.bit_xor();
    }
}
