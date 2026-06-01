//! Aggregate expressions — the typed surface for `COUNT` / `SUM` / `AVG`
//! / `MIN` / `MAX` on a [`FieldRef`].
//! # What
//! [`AggregateExpr<Out, K>`] is a `PhantomData<fn -> Out>`-tagged wrapper
//! around the [`ExprNode::Aggregate`] node. `Out` is the Rust type the
//! aggregate decodes to at fetch time:
//! | Aggregate | `Out` |
//! |------------------|----------------------------------------|
//! | `count` | `i64` |
//! | `count_star` | `i64` |
//! | `sum` | `V` (column's numeric type) |
//! | `avg` | `f64` |
//! | `min` / `max`| `V` (column's type) |
//! Every aggregate composes with the expression IR's existing walk — an
//! [`AggregateExpr<Out, K>`] holds a plain [`ExprNode::Aggregate`] node and
//! the emitter in [`super::sql::emit_expr`] lowers it to the matching
//! Postgres keyword + optional `FILTER (WHERE ...)` tail.
//! # Why typed `Out`
//! The scalar terminal ([`crate::query::aggregate::AggregateQuery::fetch_one`])
//! and the per-column decode on [`crate::query::annotate::AnnotatedQuerySet`]
//! both drive `tokio_postgres::Client::query_one` / `row.get::<_, Out>(..)`
//! the decoder needs to know the Rust type up front. `Out` is that pin: it
//! captures whatever the aggregate returns so the SELECT-list builder
//! never needs runtime type reflection. No `AggregateExpr<Any>` — the
//! compile-time bound is the whole point.
//! # Bounds on `min` / `max`
//! Rust's `Ord` is the natural "orderable" trait, but `f32` / `f64` do
//! not implement it (NaN makes total order impossible in Rust). Postgres
//! happily runs `MIN`/`MAX` on both integer and floating-point columns,
//! so the typed surface gates `min` / `max` on `postgres_types::FromSql`
//! bounds rather than Rust `Ord`. Any column whose value type decodes
//! from a Postgres scalar (`V: for<'r> postgres_types::FromSql<'r>`)
//! can be aggregated — that covers `i16`,
//! `i32`, `i64`, `f32`, `f64`, `Decimal`, `time::OffsetDateTime`,
//! `time::Date`, `String`, and the HeeRanjID PK types.
//! # Chaining `.filter(...)`
//! `AggregateExpr::filter` attaches a `FILTER (WHERE <cond>)` clause.
//! Calling `.filter(...)` twice on the same aggregate **overwrites**
//! the previous filter; users compose multi-predicate filters with the
//! expression IR's `and_with` / `or_with` helpers before handing the
//! result to `.filter(...)`. This matches the `QuerySet::limit(n)`
//! pattern where the last call wins — simplest to reason about,
//! easiest to document.
//! # Where
//! - [`super::node::ExprNode::Aggregate`] / [`super::node::AggOp`] — the
//!   untyped payload.
//! - [`super::sql::emit_expr`] — renders the SQL tokens.
//! - [`crate::query::aggregate::AggregateQuery`] — scalar terminal.
//! - [`crate::query::annotate::AnnotatedQuerySet`] — typed-tuple
//!   terminal that embeds value aggregates in the plain ungrouped SELECT list
//!   alongside `T::*`. Grouped annotate and scalar aggregate remain generic
//!   over all aggregate kinds because they do not synthesize `OVER `.

use crate::expr::Expr;
use crate::expr::arithmetic::Numeric;
use crate::expr::node::{AggOp, ExprNode};
use crate::expr::window::WindowBuilder;
use crate::model::Model;
use crate::query::field::FieldRef;
use std::marker::PhantomData;

pub(crate) mod sealed {
    pub trait Sealed {}
}

/// Compile-time marker for aggregate-modifier families.
/// The four blessed markers — [`ValueAgg`], [`MetadataAgg`],
/// [`OrderedSetAgg`], [`HypotheticalSetAgg`] — partition the aggregate
/// universe by which modifier methods are legal. Each
/// `AggregateExpr<Out, K>` carries the kind in `PhantomData`, and the
/// per-kind `impl` blocks below project the modifier surface (e.g.
/// `.distinct` lives only on `AggregateExpr<Out, ValueAgg>`, never on
/// `AggregateExpr<Out, OrderedSetAgg>`).
/// The trait is sealed via [`sealed::Sealed`]: only framework-internal
/// kind markers implement it. Downstream crates cannot name `Sealed`,
/// so external `KindEvidence` impls are unreachable — that means the
/// `AnnotationSlot for AggregateExpr<V, K> where K: KindEvidence` and
/// `QuerySet::aggregate` widenings cannot be subverted by a fabricated
/// kind tag. Plain ungrouped `QuerySet::annotate` adds one more
/// windowability gate: only `ValueAgg` can flow to the synthesized `OVER `
/// emitter, while metadata, ordered-set, and hypothetical-set kinds stay
/// legal through scalar aggregate and grouped annotate paths.
pub trait KindEvidence: sealed::Sealed {}

/// Default value-aggregate family — supports `DISTINCT`, `filter`, `over`,
/// and per-aggregate `order_by`.
pub struct ValueAgg;

/// Metadata-only aggregates such as `GROUPING` that support no modifiers.
pub struct MetadataAgg;

/// Ordered-set aggregates such as `PERCENTILE_CONT`.
pub struct OrderedSetAgg;

/// Hypothetical-set aggregate family (`RANK`, `DENSE_RANK`, etc.).
pub struct HypotheticalSetAgg;

impl sealed::Sealed for ValueAgg {}
impl sealed::Sealed for MetadataAgg {}
impl sealed::Sealed for OrderedSetAgg {}
impl sealed::Sealed for HypotheticalSetAgg {}

impl KindEvidence for ValueAgg {}
impl KindEvidence for MetadataAgg {}
impl KindEvidence for OrderedSetAgg {}
impl KindEvidence for HypotheticalSetAgg {}

/// Typed aggregate expression — the result of `f.col.count`,
/// `.sum`, `.avg`, `.min`, `.max`.
/// Carries an [`ExprNode::Aggregate`] payload plus a `PhantomData<fn ->
/// Out>` tag pinning the Rust decode type and a `PhantomData<fn -> K>`
/// tag pinning the modifier family. `#[must_use]` because a dropped
/// aggregate is usually a mistake — the user likely meant to feed it
/// into [`crate::query::QuerySet::aggregate`] or
/// [`crate::query::QuerySet::annotate`].
/// `Clone + Debug` are implemented manually rather than via
/// `#[derive(Clone, Debug)]` because the derive macro would add
/// `K: Clone` / `K: Debug` bounds on every method that takes
/// `AggregateExpr<Out, K>` by value through a `Clone`-bounded slot
/// (notably `GroupedAnnotatedQuerySet::having` / `order_by` which
/// require `IntoAggregateTuple: Clone`). The four kind markers are
/// ZSTs with no derived traits and live only in `PhantomData`, so
/// cloning / formatting an `AggregateExpr` never actually touches the
/// kind value — hand-rolling these impls keeps the typed surface
/// usable without forcing `Clone` / `Debug` onto the markers.
#[must_use = "aggregates are lazy — dropping one silently omits the column"]
pub struct AggregateExpr<Out, K = ValueAgg> {
    pub(crate) node: ExprNode,
    pub(crate) _out: PhantomData<fn() -> Out>,
    pub(crate) _kind: PhantomData<fn() -> K>,
}

// Manual `Clone` / `Debug` impls — see the rationale on `AggregateExpr`
// above. The derive macro's eager `K: Clone` / `K: Debug` bounds were
// incompatible with the kind-state ZSTs (which have no derives) and
// cascaded into compile failures on every grouped `having` / `order_by`
// path through the `A: IntoAggregateTuple + Clone` bound.
impl<Out, K> Clone for AggregateExpr<Out, K> {
    fn clone(&self) -> Self {
        AggregateExpr {
            node: self.node.clone(),
            _out: PhantomData,
            _kind: PhantomData,
        }
    }
}

impl<Out, K> std::fmt::Debug for AggregateExpr<Out, K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AggregateExpr")
            .field("node", &self.node)
            .finish()
    }
}

impl<Out, K: KindEvidence> AggregateExpr<Out, K> {
    /// Crate-private constructor. The typed aggregate methods on
    /// [`FieldRef`] are the supported entry points; downstream code
    /// cannot fabricate an arbitrarily-typed aggregate by smuggling in
    /// a raw [`ExprNode`].
    pub(crate) fn from_node(node: ExprNode) -> Self {
        AggregateExpr {
            node,
            _out: PhantomData,
            _kind: PhantomData,
        }
    }

    /// Build an `AggregateExpr<Out>` for the unary `AGG(column)` shape.
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
            within_group_order_by: Vec::new(),
        })
    }

    /// Build an `AggregateExpr<Out>` for the binary `AGG(y, x)` shape.
    /// Powers the two-arg aggregate family — `COVAR_POP`, `COVAR_SAMP`,
    /// `CORR`, the `REGR_*` regression family (T6), and the JSON-object
    /// aggregates (T9 — `JSON_OBJECT_AGG` / `JSONB_OBJECT_AGG`). Layout
    /// mirrors [`Self::unary_agg`] but populates the `arg2` slot with
    /// the second column reference.
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
            within_group_order_by: Vec::new(),
        })
    }

    /// Build an `AggregateExpr<Out>` for the ordered-set
    /// `AGG(args) WITHIN GROUP (ORDER BY target)` shape.
    /// Powers `PercentileCont` / `PercentileDisc` / `Mode`. The `arg`
    /// slot carries the function-call literal (the percentile fraction
    /// for `PercentileCont` / `PercentileDisc`, or a sentinel
    /// `Field { column: "" }` for `Mode` which takes no args). The
    /// `target` ORDER BY column is populated from the receiver
    /// `FieldRef` at construction time and lives in the
    /// `within_group_order_by` slot.
    /// T7 introduced this constructor.
    pub(crate) fn ordered_set(
        op: AggOp,
        arg: ExprNode,
        target: crate::query::order::OrderExpr,
        cast_to: Option<&'static str>,
    ) -> Self {
        AggregateExpr::from_node(ExprNode::Aggregate {
            op,
            arg: Box::new(arg),
            arg2: None,
            filter: None,
            cast_to,
            distinct: false,
            window: None,
            order_by: Vec::new(),
            within_group_order_by: vec![target],
        })
    }
}

impl<Out> AggregateExpr<Out, ValueAgg> {
    /// Attach a `FILTER (WHERE <cond>)` clause to this aggregate.
    pub fn filter(mut self, cond: Expr<bool>) -> Self {
        if let ExprNode::Aggregate { filter, .. } = &mut self.node {
            *filter = Some(Box::new(cond.node));
        }
        self
    }

    /// Apply the `DISTINCT` modifier to this aggregate, emitting
    /// `AGG(DISTINCT col)` rather than `AGG(col)`.
    /// # Rejected at compile time
    /// `.distinct` lives only on this `impl<Out> AggregateExpr<Out, ValueAgg>`
    /// block — non-value aggregate families ([`MetadataAgg`] for
    /// `GROUPING`, [`OrderedSetAgg`] for `PERCENTILE_CONT` / `PERCENTILE_DISC`
    /// / `MODE`, [`HypotheticalSetAgg`] for `RANK` / `DENSE_RANK` /
    /// `PERCENT_RANK` / `CUME_DIST`) do not expose it, so attempting to
    /// chain `.distinct` on those is a method-not-found compile error
    /// at the type-state guard (#89). No runtime check is needed for
    /// the typed surface.
    /// # Rejected at fetch time
    /// Three combinations escape the type-state and surface as
    /// [`crate::DjogiError::UnsupportedAggregate`] from
    /// [`crate::expr::sql::check_aggregate_legality`]:
    /// - `COUNT(*)` with `DISTINCT`: `COUNT(DISTINCT *)` is not valid SQL.
    ///   `count_star` shares the `ValueAgg` type-state with `count`,
    ///   `sum`, etc., so `.distinct` is callable; the runtime check
    ///   catches the COUNT-specific shape. Use `COUNT(DISTINCT col)`
    ///   via [`FieldRef::count`] instead.
    /// - `STRING_AGG(DISTINCT col, sep)` without a per-aggregate
    ///   `ORDER BY`: Postgres requires `STRING_AGG(DISTINCT col, sep
    /// ORDER BY ...)` to disambiguate the output tail. Chain
    ///   [`Self::order_by`] with a deterministic key to make the
    ///   combination well-formed.
    /// - `COUNT(*)` with a per-aggregate `ORDER BY`: the `COUNT(*)`
    ///   emitter hard-codes `COUNT(*)` and ignores the `order_by` slot,
    ///   so chaining `.order_by(...)` on `count_star` would silently
    ///   drop the modifier; the legality check rejects this at fetch
    ///   time. Chain ORDER BY at the `QuerySet` level instead, or use
    ///   `COUNT(col ORDER BY ...)` via [`FieldRef::count`].
    #[must_use = "AggregateExpr is a value — dropping discards the DISTINCT flag"]
    pub fn distinct(mut self) -> Self {
        if let ExprNode::Aggregate { distinct, .. } = &mut self.node {
            *distinct = true;
        }
        self
    }

    /// Promote this aggregate to a windowed aggregate via a [`WindowBuilder`].
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

    /// Append a per-aggregate `ORDER BY` key.
    #[must_use = "AggregateExpr is a value — dropping discards the ORDER BY"]
    pub fn order_by(mut self, ord: crate::query::order::OrderExpr) -> Self {
        if let ExprNode::Aggregate { order_by, .. } = &mut self.node {
            order_by.push(ord);
        }
        self
    }
}

impl<Out> AggregateExpr<Out, OrderedSetAgg> {
    /// Attach a `FILTER (WHERE <cond>)` clause to this aggregate.
    #[must_use = "AggregateExpr is a value — dropping discards the FILTER clause"]
    pub fn filter(mut self, cond: Expr<bool>) -> Self {
        if let ExprNode::Aggregate { filter, .. } = &mut self.node {
            *filter = Some(Box::new(cond.node));
        }
        self
    }

    /// Override the `WITHIN GROUP (ORDER BY ...)` target.
    #[must_use = "AggregateExpr is a value — dropping discards the WITHIN GROUP target"]
    pub fn within_group_order_by(mut self, target: crate::query::order::OrderExpr) -> Self {
        if let ExprNode::Aggregate {
            within_group_order_by,
            ..
        } = &mut self.node
        {
            *within_group_order_by = vec![target];
        }
        self
    }
}

impl<Out> AggregateExpr<Out, HypotheticalSetAgg> {
    /// Attach a `FILTER (WHERE <cond>)` clause to this aggregate.
    #[must_use = "AggregateExpr is a value — dropping discards the FILTER clause"]
    pub fn filter(mut self, cond: Expr<bool>) -> Self {
        if let ExprNode::Aggregate { filter, .. } = &mut self.node {
            *filter = Some(Box::new(cond.node));
        }
        self
    }

    /// Override the `WITHIN GROUP (ORDER BY ...)` target.
    #[must_use = "AggregateExpr is a value — dropping discards the WITHIN GROUP target"]
    pub fn within_group_order_by(mut self, target: crate::query::order::OrderExpr) -> Self {
        if let ExprNode::Aggregate {
            within_group_order_by,
            ..
        } = &mut self.node
        {
            *within_group_order_by = vec![target];
        }
        self
    }
}

// ── COUNT ─────────────────────────────────────────────────────────────
// `count` is available on every `FieldRef<M, V>` regardless of `V`,
// because Postgres `COUNT(col)` works on any column type (it counts
// non-null values). `count_star` is an associated function because it
// does not need a column reference — users call it as
// `AggregateExpr::<i64>::count_star` or, from a field-closure context,
// build it manually by reaching into the `ExprNode` (which they cannot
// the enum is crate-private). Task 4 ships `.count` only; the
// `COUNT(*)` variant is exposed through `FieldRef::count_star` as an
// inherent method that uses any FieldRef to satisfy the receiver but
// renders as `COUNT(*)`.

impl<M: Model, V> FieldRef<M, V> {
    /// `COUNT(column)` — returns `i64`.
    /// Counts rows where the column is non-null. For a total row count
    /// that ignores NULL status, use [`FieldRef::count_star`] (which
    /// emits `COUNT(*)`).
    /// `COUNT` in Postgres always returns `BIGINT`, which decodes
    /// directly into `i64` — no cast needed.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn count(self) -> AggregateExpr<i64> {
        AggregateExpr::unary_agg(AggOp::Count, self.column(), None)
    }

    /// `COUNT(*)` — returns `i64`.
    /// Counts every row in the (grouped) relation, including those
    /// where every column is NULL. Routes through a dedicated
    /// [`AggOp::CountStar`] variant rather than
    /// `ExprNode::Field { column: "*" }` so the bare `*` never
    /// reaches the identifier-validation pass in
    /// [`crate::ident::assert_plain_ident`] nor the column-
    /// qualification pass that select_related adds.
    /// `FieldRef<M, V>` is the receiver because `AggregateExpr`
    /// constructors live on `FieldRef` by convention — the receiver's
    /// `column` is **not** used for `COUNT(*)` (the emitter ignores
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
// Gated on the sealed `Numeric` trait from `expr::arithmetic` — same
// seal that gates `+` / `-` / `*` / `/` on `Expr<T>`. ships
// `i16 / i32 / i64 / f32 / f64`; `Decimal` extends the trait in .

impl<M: Model, V: Numeric> FieldRef<M, V> {
    /// `SUM(column)` — returns `V`.
    /// Sums non-null values of the column. Gated on the sealed
    /// [`Numeric`] trait so only framework-blessed numeric types
    /// compose — `sum` on a `String` column is a compile error, not a
    /// runtime SQL error.
    /// # Postgres widening vs `Out = V`
    /// Postgres widens integer sums — `SUM(BIGINT)` returns `NUMERIC`,
    /// `SUM(SMALLINT)` returns `BIGINT`. The typed surface keeps
    /// `Out = V` for ergonomics (most call sites sum into the same
    /// scalar type they declared on the column), and the emitter
    /// narrows the result back with an explicit `::<V::SUM_CAST>`
    /// cast so the decoder can return `V` directly.
    /// This means a sum that overflows the original column's range
    /// raises a `numeric_value_out_of_range` error at query time
    /// Postgres refuses to truncate on the narrowing cast. That is
    /// deliberate: silent truncation would be worse than an error.
    /// Users aggregating values that exceed `V::MAX` should declare a
    /// larger column type or use `ctx.raw_scalar` for a `NUMERIC` /
    /// `Decimal` decode; the `Decimal` `Numeric` impl is the
    /// framework-supported path for precision-critical sums.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn sum(self) -> AggregateExpr<V> {
        // `V::SUM_CAST` — associated constant on the sealed `Numeric`
        // trait. Each blessed numeric type names its own Postgres cast
        // target there.
        AggregateExpr::unary_agg(AggOp::Sum, self.column(), Some(<V as Numeric>::SUM_CAST))
    }

    /// `AVG(column)` — returns `f64`.
    /// Averages non-null values. Postgres returns `NUMERIC` for
    /// integer inputs and `DOUBLE PRECISION` for floating-point
    /// inputs; the typed surface pins `Out = f64` for both by
    /// emitting an explicit `::DOUBLE PRECISION` cast so the decoder
    /// returns uniformly `f64`. Callers who need `Decimal`-precision
    /// averages use `ctx.raw_scalar` until the `Decimal` support
    /// lands.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn avg(self) -> AggregateExpr<f64> {
        // Always DOUBLE PRECISION regardless of the input numeric type
        // the typed surface's `Out = f64` promise holds uniformly.
        AggregateExpr::unary_agg(AggOp::Avg, self.column(), Some(<V as Numeric>::AVG_CAST))
    }

    /// `STDDEV_POP(column)` — population standard deviation, returned as
    /// `f64`.
    /// Postgres returns `NUMERIC` for integer inputs and `DOUBLE PRECISION`
    /// for floating-point inputs; the explicit `::DOUBLE PRECISION` cast
    /// narrows uniformly to `f64` so the typed surface's `Out = f64`
    /// promise holds across all blessed numeric column types. Use this
    /// when you have data for the entire population and want the exact
    /// dispersion measure (no sample-correction `n-1` term).
    /// # Empty / single-row groups
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
    /// Uses the Bessel-corrected formula (divides by `n-1`), the standard
    /// choice when treating the rows as a sample of a larger population.
    /// Empty groups and single-row groups return `NULL` (Postgres divides
    /// by zero in the latter case).
    /// See [`FieldRef::stddev`] for the alias spelling and
    /// [`FieldRef::stddev_pop`] for the population form.
    /// # Example
    /// ```ignore
    /// // Per-org sample stddev of order amounts
    /// let scatter: Vec<(i64, f64)> = Order::objects()
    ///     .group_by(|f| f.org_id())
    ///     .annotate(|f| f.amount().stddev_samp())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn stddev_samp(self) -> AggregateExpr<f64> {
        AggregateExpr::unary_agg(AggOp::StddevSamp, self.column(), Some("DOUBLE PRECISION"))
    }

    /// `STDDEV(column)` — Postgres alias for [`FieldRef::stddev_samp`].
    /// Both produce identical results; the emitter preserves the spelling
    /// the caller used (matching the [`FieldRef::every`] alias treatment).
    /// Adopters reading SQL-standard docs reach for `STDDEV_SAMP`;
    /// adopters reading `pg` docs typically write `STDDEV`. Both
    /// work, both round-trip exactly as written.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn stddev(self) -> AggregateExpr<f64> {
        AggregateExpr::unary_agg(AggOp::Stddev, self.column(), Some("DOUBLE PRECISION"))
    }

    /// `VAR_POP(column)` — population variance, returned as `f64`.
    /// Population form (no `n-1` correction). Same NULL-on-empty-group
    /// behaviour as the stddev pair; same `DOUBLE PRECISION` narrowing
    /// cast applies.
    /// # Example
    /// ```ignore
    /// // Population variance of latency across the day's request stream
    /// let var: f64 = Request::objects()
    ///     .filter(|r| r.day().eq(today))
    ///     .aggregate(|r| r.latency_ms().var_pop())
    ///     .fetch_one(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn var_pop(self) -> AggregateExpr<f64> {
        AggregateExpr::unary_agg(AggOp::VarPop, self.column(), Some("DOUBLE PRECISION"))
    }

    /// `VAR_SAMP(column)` — sample variance, returned as `f64`.
    /// Bessel-corrected (`n-1`) form. Returns `NULL` for empty groups
    /// and for single-row groups (division by zero on `n-1`).
    /// See [`FieldRef::variance`] for the alias spelling and
    /// [`FieldRef::var_pop`] for the population form.
    /// # Example
    /// ```ignore
    /// // Per-region sample variance of customer order totals
    /// let dispersion: Vec<(i64, f64)> = Order::objects()
    ///     .group_by(|f| f.region_id())
    ///     .annotate(|f| f.amount().var_samp())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn var_samp(self) -> AggregateExpr<f64> {
        AggregateExpr::unary_agg(AggOp::VarSamp, self.column(), Some("DOUBLE PRECISION"))
    }

    /// `VARIANCE(column)` — Postgres alias for [`FieldRef::var_samp`].
    /// Same spelling-preservation contract as [`FieldRef::stddev`] /
    /// [`FieldRef::every`].
    /// # Example
    /// ```ignore
    /// let var: f64 = Order::objects()
    ///     .aggregate(|f| f.amount().variance())
    ///     .fetch_one(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn variance(self) -> AggregateExpr<f64> {
        AggregateExpr::unary_agg(AggOp::Variance, self.column(), Some("DOUBLE PRECISION"))
    }

    /// `COVAR_POP(y, x)` — population covariance, returned as `f64`.
    /// Self is `y` (dependent variable); the argument is `x`
    /// (independent variable). This matches Postgres' convention across
    /// the regression / covariance family — the `y` column is always
    /// the first argument.
    /// Cast to `DOUBLE PRECISION` so the typed surface's `Out = f64`
    /// promise holds for any combination of integer / float column
    /// types on either side. Both sides gate on the sealed `Numeric`
    /// trait — `covar_pop` between a `String` column and an `i64`
    /// column is a compile error, not a runtime Postgres type error.
    /// # Example
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
    /// Result range is `[-1.0, 1.0]`: `1.0` for perfect positive linear
    /// correlation, `-1.0` for perfect negative, `0.0` for no linear
    /// relationship. Same `y, x` argument convention and
    /// `DOUBLE PRECISION` cast as [`FieldRef::covar_pop`].
    /// # Empty / single-row groups
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

    /// `REGR_AVGX(y, x)` — average of the independent variable across
    /// rows where both `y` and `x` are non-null. Returned as `f64`.
    /// Receiver is `y`, argument is `x` — Postgres convention for the
    /// regression family. Same `DOUBLE PRECISION` cast as the rest of
    /// the binary numeric aggregates.
    /// Returns `NULL` for empty groups (no (y, x) pairs survive the
    /// non-null filter).
    /// # Example
    /// ```ignore
    /// // Average ad-spend (x) on days that produced any conversions (y)
    /// let mean_x: f64 = Day::objects()
    ///     .aggregate(|f| f.conversions().regr_avgx(f.ad_spend()))
    ///     .fetch_one(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn regr_avgx<V2: Numeric>(self, x: FieldRef<M, V2>) -> AggregateExpr<f64> {
        AggregateExpr::binary_agg(
            AggOp::RegrAvgx,
            self.column(),
            x.column(),
            Some("DOUBLE PRECISION"),
        )
    }

    /// `REGR_AVGY(y, x)` — average of the dependent variable across
    /// rows where both `y` and `x` are non-null. Returned as `f64`.
    /// Same convention as [`FieldRef::regr_avgx`].
    /// Returns `NULL` for empty groups.
    /// # Example
    /// ```ignore
    /// // Average response time (y) on rows where load (x) is also recorded
    /// let mean_y: f64 = Sample::objects()
    ///     .aggregate(|f| f.response_ms().regr_avgy(f.load()))
    ///     .fetch_one(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn regr_avgy<V2: Numeric>(self, x: FieldRef<M, V2>) -> AggregateExpr<f64> {
        AggregateExpr::binary_agg(
            AggOp::RegrAvgy,
            self.column(),
            x.column(),
            Some("DOUBLE PRECISION"),
        )
    }

    /// `REGR_COUNT(y, x)` — number of input rows where both `y` and
    /// `x` are non-null. Returned as `i64`.
    /// Unlike the rest of the regression family, Postgres returns
    /// `BIGINT` here — the typed surface returns `AggregateExpr<i64>`
    /// to match. The cast slot uses `BIGINT` for emission uniformity
    /// with the unary `count` path.
    /// Returns `0` (not NULL) for empty groups — `BIGINT` count
    /// aggregates have a defined zero identity.
    /// # Example
    /// ```ignore
    /// // How many (y, x) pairs went into the regression?
    /// let n: i64 = Sample::objects()
    ///     .aggregate(|f| f.response_ms().regr_count(f.load()))
    ///     .fetch_one(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn regr_count<V2: Numeric>(self, x: FieldRef<M, V2>) -> AggregateExpr<i64> {
        AggregateExpr::binary_agg(AggOp::RegrCount, self.column(), x.column(), Some("BIGINT"))
    }

    /// `REGR_INTERCEPT(y, x)` — y-intercept of the least-squares-fit
    /// line through the (y, x) pairs. Returned as `f64`.
    /// Returns `NULL` for empty groups and groups where `x` has zero
    /// variance (the regression line is undefined).
    /// # Example
    /// ```ignore
    /// // Per-region regression intercept of conversions on ad-spend
    /// let intercepts: Vec<(i64, f64)> = Day::objects()
    ///     .group_by(|f| f.region_id())
    ///     .annotate(|f| f.conversions().regr_intercept(f.ad_spend()))
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn regr_intercept<V2: Numeric>(self, x: FieldRef<M, V2>) -> AggregateExpr<f64> {
        AggregateExpr::binary_agg(
            AggOp::RegrIntercept,
            self.column(),
            x.column(),
            Some("DOUBLE PRECISION"),
        )
    }

    /// `REGR_R2(y, x)` — coefficient of determination of the
    /// least-squares-fit line. Returned as `f64`.
    /// Range is `[0.0, 1.0]`: `1.0` for a perfect fit, `0.0` for no
    /// linear relationship. Returns `NULL` when the group is empty or
    /// `x` has zero variance.
    /// # Example
    /// ```ignore
    /// // How well does ad-spend explain conversions in each region?
    /// let r2: Vec<(i64, f64)> = Day::objects()
    ///     .group_by(|f| f.region_id())
    ///     .annotate(|f| f.conversions().regr_r2(f.ad_spend()))
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn regr_r2<V2: Numeric>(self, x: FieldRef<M, V2>) -> AggregateExpr<f64> {
        AggregateExpr::binary_agg(
            AggOp::RegrR2,
            self.column(),
            x.column(),
            Some("DOUBLE PRECISION"),
        )
    }

    /// `REGR_SLOPE(y, x)` — slope of the least-squares-fit line
    /// through the (y, x) pairs. Returned as `f64`.
    /// Same NULL behaviour as [`FieldRef::regr_intercept`].
    /// # Example
    /// ```ignore
    /// // Per-region slope: how much does each $ of ad-spend buy in conversions?
    /// let slopes: Vec<(i64, f64)> = Day::objects()
    ///     .group_by(|f| f.region_id())
    ///     .annotate(|f| f.conversions().regr_slope(f.ad_spend()))
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn regr_slope<V2: Numeric>(self, x: FieldRef<M, V2>) -> AggregateExpr<f64> {
        AggregateExpr::binary_agg(
            AggOp::RegrSlope,
            self.column(),
            x.column(),
            Some("DOUBLE PRECISION"),
        )
    }

    /// `REGR_SXX(y, x)` — sum of squares of the independent variable
    /// across the (y, x) pairs (`SUM((x - AVG(x))^2)`). Returned as
    /// `f64`. Useful as the denominator in slope / r² calculations
    /// when computing the regression manually.
    /// Returns `NULL` for empty groups; returns `0.0` when every (y, x)
    /// pair has the same `x` (zero variance).
    /// # Example
    /// ```ignore
    /// let sxx: f64 = Sample::objects()
    ///     .aggregate(|f| f.y().regr_sxx(f.x()))
    ///     .fetch_one(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn regr_sxx<V2: Numeric>(self, x: FieldRef<M, V2>) -> AggregateExpr<f64> {
        AggregateExpr::binary_agg(
            AggOp::RegrSxx,
            self.column(),
            x.column(),
            Some("DOUBLE PRECISION"),
        )
    }

    /// `REGR_SXY(y, x)` — sum of products of (y, x) deviations
    /// across the pairs (`SUM((x - AVG(x)) * (y - AVG(y)))`).
    /// Returned as `f64`. Useful as the numerator in slope / covariance
    /// calculations.
    /// Returns `NULL` for empty groups.
    /// # Example
    /// ```ignore
    /// // Sum of cross-deviations — input to manual covariance computation
    /// let sxy: f64 = Sample::objects()
    ///     .aggregate(|f| f.y().regr_sxy(f.x()))
    ///     .fetch_one(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn regr_sxy<V2: Numeric>(self, x: FieldRef<M, V2>) -> AggregateExpr<f64> {
        AggregateExpr::binary_agg(
            AggOp::RegrSxy,
            self.column(),
            x.column(),
            Some("DOUBLE PRECISION"),
        )
    }

    /// `REGR_SYY(y, x)` — sum of squares of the dependent variable
    /// across the (y, x) pairs (`SUM((y - AVG(y))^2)`). Returned as
    /// `f64`. Useful as the denominator in r² calculations when
    /// computing the regression manually.
    /// Returns `NULL` for empty groups; returns `0.0` when every (y, x)
    /// pair has the same `y` (zero variance on the dependent side).
    /// # Example
    /// ```ignore
    /// let syy: f64 = Sample::objects()
    ///     .aggregate(|f| f.y().regr_syy(f.x()))
    ///     .fetch_one(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn regr_syy<V2: Numeric>(self, x: FieldRef<M, V2>) -> AggregateExpr<f64> {
        AggregateExpr::binary_agg(
            AggOp::RegrSyy,
            self.column(),
            x.column(),
            Some("DOUBLE PRECISION"),
        )
    }
}

// ── MIN / MAX ─────────────────────────────────────────────────────────
// Bound on `V: IntoFilterValue` — every SQL-bindable type Djogi ships
// with already implements that trait (see `query::field::IntoFilterValue`).
// Mirroring that bound here keeps the set of MIN/MAX-able columns in
// lockstep with the set of filter-able columns: one seal to extend
// when a future phase adds a new column type (e.g. `Decimal` in
// ). Rust `Ord` is deliberately not used — `f64` doesn't
// implement it, but Postgres `MIN(col)` / `MAX(col)` on a `DOUBLE
// PRECISION` column is a routine query.

impl<M: Model, V> FieldRef<M, V>
where
    V: crate::query::field::IntoFilterValue,
{
    /// `MIN(column)` — returns `V`.
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
    /// Returns the largest non-null value of the column. Same bound
    /// rationale as [`FieldRef::min`].
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn max(self) -> AggregateExpr<V> {
        AggregateExpr::unary_agg(AggOp::Max, self.column(), None)
    }
}

// ── ARRAY_AGG / JSON_AGG ───────────────────────────────────────────────────
// Available on every `FieldRef<M, V>` regardless of `V` — Postgres can
// ARRAY_AGG / JSONB_AGG any column type. The return type:
// - `array_agg` → `AggregateExpr<Vec<V>>`: the annotate decode path calls
// `row.try_get::<_, Vec<V>>(alias)`, which postgres-types handles via its
// built-in array decoding when `V: FromSql`.
// - `json_agg` → `AggregateExpr<serde_json::Value>`: JSONB_AGG always
// produces a JSON array; decoding into `serde_json::Value` covers every
// element type without requiring `V`-specific codec knowledge.

impl<M: Model, V> FieldRef<M, V> {
    /// `ARRAY_AGG(column)` — collects non-null column values into a Postgres
    /// array, returned as `Vec<V>` at the Rust level.
    /// postgres-types decodes a Postgres array column into `Vec<V>` when `V`
    /// implements `FromSql`; all scalar column types Djogi ships satisfy that
    /// bound. If `V` does not implement `FromSql`, the failure is a runtime
    /// decode error at fetch time, not a compile error here, because
    /// `FieldRef` is constructed at macro-expansion time with a type the
    /// framework knows is decodable.
    /// The aggregate emits `ARRAY_AGG(column)` without any narrowing cast.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn array_agg(self) -> AggregateExpr<Vec<V>> {
        AggregateExpr::unary_agg(AggOp::ArrayAgg, self.column(), None)
    }

    /// `JSONB_AGG(column)` — aggregates column values into a JSON array,
    /// returned as `serde_json::Value`.
    /// Djogi standardises on JSONB for all JSON storage and wire formats
    /// (see `docs/spec/decisions.md`), so `JSONB_AGG` is emitted rather
    /// than `JSON_AGG`. The returned `serde_json::Value` is always a
    /// `Value::Array` wrapping the per-row column values; callers can
    /// pattern-match or call `.as_array` to iterate.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn json_agg(self) -> AggregateExpr<serde_json::Value> {
        AggregateExpr::unary_agg(AggOp::JsonAgg, self.column(), None)
    }

    /// `JSON_OBJECT_AGG(key, value)` — builds a JSON object (Postgres
    /// `json` type) from per-row key/value tuples. Returned as
    /// `serde_json::Value` (always a `Value::Object`).
    /// Receiver is the key column, argument is the value column
    /// `f.id.json_object_agg(f.name)` emits
    /// `JSON_OBJECT_AGG(id, name)`.
    /// # Why `serde_json::Value` and not `Jsonb<T>`
    /// `Jsonb<T>` is a typed schema wrapper — adopters declare the
    /// expected shape `T` at column-definition time, and the wrapper
    /// validates incoming data against it on every save. The aggregate
    /// produces a JSON object whose shape depends entirely on the row
    /// stream feeding it (whatever `(key, value)` types the closure
    /// supplies); there is no compile-time `T` to validate against
    /// without forcing every call site to declare a one-off schema
    /// type. `serde_json::Value` is the open-shape escape hatch:
    /// adopters who know the expected shape can `serde_json::from_value`
    /// the result into their own typed struct at the call site.
    /// # Why exposed alongside [`FieldRef::jsonb_object_agg`]
    /// Djogi standardises on JSONB for storage and wire formats (see
    /// `docs/spec/decisions.md`), but adopters consuming the output
    /// from an external system that requires `json` rather than
    /// `jsonb` (rare but real — some legacy clients, certain extension
    /// surfaces) have no other in-Djogi path. This variant emits the
    /// literal `JSON_OBJECT_AGG` keyword; the
    /// [`FieldRef::jsonb_object_agg`] sibling emits `JSONB_OBJECT_AGG`.
    /// Both decode into `serde_json::Value` at the Rust level for the
    /// reason above.
    /// # Duplicate keys
    /// Postgres' `JSON_OBJECT_AGG` rejects duplicate keys at runtime
    /// the call raises `22023 (invalid_parameter_value)`. Callers
    /// guaranteeing uniqueness can pre-DISTINCT the row set or use
    /// `.filter(...)`; otherwise the JSONB variant is more forgiving
    /// (Postgres treats later keys as overwriting earlier ones for
    /// `JSONB_OBJECT_AGG`, by `jsonb`'s deduplication semantics).
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn json_object_agg<V2>(self, value: FieldRef<M, V2>) -> AggregateExpr<serde_json::Value> {
        AggregateExpr::binary_agg(AggOp::JsonObjectAgg, self.column(), value.column(), None)
    }

    /// `JSONB_OBJECT_AGG(key, value)` — `jsonb` variant of
    /// [`FieldRef::json_object_agg`]. Same shape, different Postgres
    /// return type (`jsonb` rather than `json`). Returned as
    /// `serde_json::Value`.
    /// **Recommended default** when storing or wire-serialising the
    /// aggregate result — Djogi standardises on JSONB across the
    /// framework. Use [`FieldRef::json_object_agg`] only when an
    /// external consumer specifically requires `json`.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn jsonb_object_agg<V2>(self, value: FieldRef<M, V2>) -> AggregateExpr<serde_json::Value> {
        AggregateExpr::binary_agg(AggOp::JsonbObjectAgg, self.column(), value.column(), None)
    }

    /// `GROUPING(column)` — returns `1` if `column` was rolled up in the
    /// current row (i.e. the row is a subtotal produced by `ROLLUP` /
    /// `CUBE` / `GROUPING SETS`), `0` otherwise. Returns
    /// `AggregateExpr<i32>` because Postgres' return type is `INTEGER`.
    /// # When to use
    /// Pair with the grouping-set surface (T11 will land typed
    /// `ROLLUP` / `CUBE` / `GROUPING SETS` builders) to distinguish
    /// subtotal rows from base-fact rows in the result set. Used inside
    /// SELECT / HAVING when reporting hierarchical summaries:
    /// ```ignore
    /// // SELECT region, dept, SUM(sales),
    /// //        GROUPING(region) AS is_region_subtotal,
    /// //        GROUPING(dept)   AS is_dept_subtotal
    /// // FROM   sales
    /// // GROUP BY ROLLUP(region, dept);
    /// Sales::objects()
    ///     .rollup(|f| (f.region(), f.dept()))
    ///     .annotate(|f| (
    ///         f.sales().sum(),
    ///         f.region().grouping(),
    ///         f.dept().grouping(),
    ///     ))
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    /// # Variadic form
    /// Postgres also accepts `GROUPING(c1, c2, …, cN)` returning a
    /// bitmask. Use the free function [`crate::grouping_of`] for that
    /// shape — bit 0 (LSB) maps to the rightmost argument; each bit
    /// is `1` when that column was rolled up. Implemented in #94.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn grouping(self) -> AggregateExpr<i32, MetadataAgg> {
        AggregateExpr::unary_agg(AggOp::Grouping, self.column(), None)
    }
}

/// `GROUPING(c1, c2, …, cN)` — variadic bitmask form.
/// Returns `AggregateExpr<i32>` carrying a bitmask. Postgres assigns
/// bit `0` (the least-significant bit) to the **rightmost** argument
/// and bit `N-1` to the leftmost. Each bit is `1` when that column
/// was rolled up in the current row under `GROUP BY ROLLUP` / `CUBE`
/// / `GROUPING SETS`, else `0`. With three columns `(a, b, c)`, `c`
/// maps to bit 0 (LSB): the row that rolls up `c` only yields
/// bitmask `0b001 == 1`; the row that rolls up `a` and `c` yields
/// `0b101 == 5`.
/// Postgres returns `INTEGER` for this form regardless of input
/// column types — the bitmask is positional, not value-derived
/// so the typed surface pins `Out = i32`.
/// # Why a free function and not a method on `FieldRef`
/// The columns flagged by a single `GROUPING(...)` call can have
/// different value types (`GROUPING(region, dept_id)` where one
/// is `String` and the other `i64`). A method on `FieldRef<M, V>`
/// would either need a marker trait + tuple machinery (compile-
/// time cost without a usability win) or accept `&[&'static str]`
/// just like this free function — at which point the receiver
/// becomes uninformative. Keeping the variadic constructor as a
/// free function avoids paying for tuple-of-fields plumbing that
/// no other aggregate uses.
/// # Panics
/// Panics if `columns` is empty — Postgres rejects `GROUPING`
/// with no args. The panic surfaces the framework-bug at the
/// construction site rather than at fetch time as a Postgres
/// syntax error. Also panics (via [`crate::ident::assert_plain_ident`])
/// if any column name is not a plain SQL identifier (ASCII letter
/// or underscore followed by ASCII alphanumerics or underscores,
/// up to 63 bytes) — same contract every other framework-baked
/// `&'static str` column reference upholds.
/// # Example
/// ```ignore
/// // SELECT region, dept, SUM(sales),
/// //        GROUPING(region, dept) AS subtotal_bitmask
/// // FROM   sales
/// // GROUP BY ROLLUP(region, dept);
/// use djogi::prelude::*;
/// use djogi::grouping_of;
/// Sales::objects()
///     .rollup(|f| (f.region(), f.dept()))
///     .annotate(|_f| (
///         /* ... sum ... */,
///         grouping_of(&["region", "dept"]),
///     ))
///     .fetch_all(&mut ctx).await?;
/// ```
#[must_use = "aggregates are lazy — dropping one silently omits the column"]
pub fn grouping_of(columns: &[&'static str]) -> AggregateExpr<i32, MetadataAgg> {
    assert!(
        !columns.is_empty(),
        "djogi::grouping_of: column list must be non-empty — Postgres rejects GROUPING() with no args"
    );
    let args: Vec<crate::expr::node::ExprNode> = columns
        .iter()
        .map(|&col| {
            crate::ident::assert_plain_ident(col, "GROUPING column");
            crate::expr::node::ExprNode::Field { column: col }
        })
        .collect();
    AggregateExpr::from_node(crate::expr::node::ExprNode::GroupingVariadic { args })
}

// ── PERCENTILE_CONT / PERCENTILE_DISC / MODE — ordered-set aggregates ────────
// T7. These are Postgres ordered-set aggregates: the function
// takes a literal value (the percentile fraction; or empty for `mode`)
// and pairs it with a mandatory `WITHIN GROUP (ORDER BY column)` clause
// that names the column being percentiled / mode-aggregated.
// The IR layout: the `arg` slot stores the function-call literal; the
// `within_group_order_by` slot stores the column being aggregated. The
// emitter renders `OP(arg) WITHIN GROUP (ORDER BY target)`.
// Postgres rules these aggregates honour, all enforced at compile time
// by the [`OrderedSetAgg`] kind-state (#89):
// - DISTINCT is invalid — `.distinct` is not exposed on
// `AggregateExpr<Out, OrderedSetAgg>`.
// - The in-paren `order_by` modifier (T1) is invalid — `.order_by(...)`
// is not exposed on `AggregateExpr<Out, OrderedSetAgg>`.
// - The window modifier (T3) is invalid — `.over(...)` is not exposed
// on `AggregateExpr<Out, OrderedSetAgg>`.
// - Plain ungrouped `QuerySet::annotate(...)` is also invalid for this
// family because that terminal would synthesize `OVER ` even when the
// user cannot call `.over(...)`. The `PlainAnnotationTuple` bound rejects
// ordered-set aggregates there while scalar `QuerySet::aggregate(...)` and
// grouped annotate remain generic over `K`.
// - WITHIN GROUP is mandatory — the typed `AggregateExpr::ordered_set`
// constructor populates the `within_group_order_by` slot at build
// time from the receiver column, and `.within_group_order_by(...)`
// replaces (never empties) it. The runtime `debug_assert!` in
// `check_aggregate_legality` catches future direct-IR construction
// that bypasses the typed surface.
// - FILTER (WHERE ...) is valid and exposed via `.filter(...)`.
// `percentile_cont` is `Numeric`-gated because Postgres returns
// `DOUBLE PRECISION` for numeric inputs and `INTERVAL` for interval
// inputs; the typed surface pins `Out = f64` and emits a
// `DOUBLE PRECISION` cast. `percentile_disc` and `mode` return the
// column type — the typed surface carries `Out = V` and gates on
// `IntoFilterValue` (the same bound as `min` / `max`).

impl<M: Model, V: crate::expr::arithmetic::Numeric> FieldRef<M, V> {
    /// `PERCENTILE_CONT(p) WITHIN GROUP (ORDER BY <col>)` — continuous
    /// percentile with linear interpolation between adjacent values.
    /// Returns `AggregateExpr<f64>`.
    /// `p` must be in `[0.0, 1.0]`. Postgres rejects out-of-range values
    /// at runtime with a typed error; Djogi binds `p` as a literal so
    /// the emitted SQL carries it in the function-call arg slot.
    /// The receiver column becomes the `WITHIN GROUP (ORDER BY ...)`
    /// target with default ASC direction. Override via
    /// [`AggregateExpr::within_group_order_by`] if a different column or
    /// DESC direction is needed.
    /// # Postgres NULL behaviour
    /// Empty groups produce SQL NULL — the non-`Option` typed surface
    /// treats that as a runtime error. Wrap `Out = Option<f64>` at the
    /// call site for NULL-safe handling.
    /// # Example
    /// ```ignore
    /// // Median request latency per service
    /// let medians: Vec<(ServiceId, f64)> = Request::objects()
    ///     .group_by(|f| f.service_id())
    ///     .annotate(|f| f.latency_ms().percentile_cont(0.5))
    ///     .fetch_all(&mut ctx).await?;
    ///
    /// // p95 latency
    /// let p95: f64 = Request::objects()
    ///     .filter(|r| r.day().eq(today))
    ///     .aggregate(|r| r.latency_ms().percentile_cont(0.95))
    ///     .fetch_one(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn percentile_cont(self, p: f64) -> AggregateExpr<f64, OrderedSetAgg> {
        let target = self.asc();
        AggregateExpr::ordered_set(
            AggOp::PercentileCont,
            ExprNode::Literal(crate::query::condition::FilterValue::F64(p)),
            target,
            Some("DOUBLE PRECISION"),
        )
    }
}

impl<M: Model, V> FieldRef<M, V>
where
    V: crate::query::field::IntoFilterValue,
{
    /// `PERCENTILE_DISC(p) WITHIN GROUP (ORDER BY <col>)` — discrete
    /// percentile (no interpolation; returns the actual value at the
    /// percentile cut). Returns `AggregateExpr<V>` — the column's own
    /// type, since Postgres returns the actual data point.
    /// Use when the column type can't be linearly interpolated
    /// (categorical / ordinal / non-numeric data) or when adopters need
    /// the actual row value rather than an interpolated approximation.
    /// Same WITHIN GROUP target population as
    /// [`FieldRef::percentile_cont`] — receiver column at default ASC.
    /// # Postgres NULL behaviour
    /// Empty groups produce SQL NULL.
    /// # Example
    /// ```ignore
    /// // Discrete median order amount (returns an actual order's
    /// // amount, not an interpolated value).
    /// let median_amount: i64 = Order::objects()
    ///     .aggregate(|f| f.amount().percentile_disc(0.5))
    ///     .fetch_one(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn percentile_disc(self, p: f64) -> AggregateExpr<V, OrderedSetAgg> {
        let target = self.asc();
        AggregateExpr::ordered_set(
            AggOp::PercentileDisc,
            ExprNode::Literal(crate::query::condition::FilterValue::F64(p)),
            target,
            None,
        )
    }

    /// `MODE WITHIN GROUP (ORDER BY <col>)` — most common value in the
    /// group. Returns `AggregateExpr<V>` — the column's own type.
    /// Ties: Postgres returns the first value encountered in the
    /// `WITHIN GROUP (ORDER BY ...)` ordering. Default ASC means ties
    /// resolve to the smaller value; flip via
    /// [`AggregateExpr::within_group_order_by`] passing
    /// `self.desc` to bias toward the larger.
    /// # Postgres NULL behaviour
    /// Empty groups (or all-NULL inputs) produce SQL NULL — `MODE`
    /// over NULLs has no defined value.
    /// # Example
    /// ```ignore
    /// // Most common payment method per region
    /// let popular: Vec<(RegionId, String)> = Order::objects()
    ///     .group_by(|f| f.region_id())
    ///     .annotate(|f| f.payment_method().mode())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn mode(self) -> AggregateExpr<V, OrderedSetAgg> {
        // Mode takes no function arguments — the arg slot stores a
        // sentinel placeholder (parallel to CountStar). The emitter
        // renders `MODE` and ignores arg on this branch.
        let target = self.asc();
        AggregateExpr::ordered_set(AggOp::Mode, ExprNode::Field { column: "" }, target, None)
    }

    /// `RANK(value) WITHIN GROUP (ORDER BY <col>)` — hypothetical-set
    /// rank: the rank that `value` would have if inserted into the
    /// sorted column. Returns `AggregateExpr<i64>`.
    /// # Distinct from the window-form rank
    /// Postgres has two `RANK` functions:
    /// - **Window form** ([`crate::expr::Rank`]) — ranks each row within
    ///   a `PARTITION BY ... ORDER BY ...` window.
    /// - **Hypothetical-set form** (this method) — answers "what rank
    ///   would this hypothetical value have in the sorted set?" without
    ///   inserting the row. The argument matches the column type.
    /// # Postgres NULL behaviour
    /// Empty groups produce SQL NULL.
    /// # Example
    /// ```ignore
    /// // What rank would a $7,500 salary have among the engineering team?
    /// let rank: i64 = Employee::objects()
    ///     .filter(|e| e.dept().eq("engineering"))
    ///     .aggregate(|e| e.salary().rank_of(7_500))
    ///     .fetch_one(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn rank_of(self, value: V) -> AggregateExpr<i64, HypotheticalSetAgg> {
        let target = self.asc();
        let arg = ExprNode::Literal(value.into_filter_value());
        AggregateExpr::ordered_set(AggOp::HypotheticalRank, arg, target, Some("BIGINT"))
    }

    /// `DENSE_RANK(value) WITHIN GROUP (ORDER BY <col>)`
    /// hypothetical-set dense rank (ties don't leave gaps in rank
    /// numbering). Returns `AggregateExpr<i64>`.
    /// Same shape and rationale as [`Self::rank_of`]; differs only in
    /// tie-handling semantics.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn dense_rank_of(self, value: V) -> AggregateExpr<i64, HypotheticalSetAgg> {
        let target = self.asc();
        let arg = ExprNode::Literal(value.into_filter_value());
        AggregateExpr::ordered_set(AggOp::HypotheticalDenseRank, arg, target, Some("BIGINT"))
    }

    /// `PERCENT_RANK(value) WITHIN GROUP (ORDER BY <col>)`
    /// hypothetical-set percent rank: the position the hypothetical
    /// value would occupy as a fraction in `[0.0, 1.0]`. Returns
    /// `AggregateExpr<f64>`.
    /// Distinct from the window-form `PERCENT_RANK` (which doesn't
    /// take a hypothetical arg and operates over a window partition).
    /// # Example
    /// ```ignore
    /// // What percentile (as a fraction) would a 100 ms latency be at?
    /// let pct: f64 = Request::objects()
    ///     .aggregate(|r| r.latency_ms().percent_rank_of(100.0))
    ///     .fetch_one(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn percent_rank_of(self, value: V) -> AggregateExpr<f64, HypotheticalSetAgg> {
        let target = self.asc();
        let arg = ExprNode::Literal(value.into_filter_value());
        AggregateExpr::ordered_set(
            AggOp::HypotheticalPercentRank,
            arg,
            target,
            Some("DOUBLE PRECISION"),
        )
    }

    /// `CUME_DIST(value) WITHIN GROUP (ORDER BY <col>)`
    /// hypothetical-set cumulative distribution: the fraction of rows
    /// that would rank at or below the hypothetical value. Returns
    /// `AggregateExpr<f64>`.
    /// Distinct from the window-form `CUME_DIST`.
    /// # Example
    /// ```ignore
    /// // What fraction of orders are at or below a $500 amount?
    /// let pct: f64 = Order::objects()
    ///     .aggregate(|o| o.amount().cume_dist_of(500))
    ///     .fetch_one(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn cume_dist_of(self, value: V) -> AggregateExpr<f64, HypotheticalSetAgg> {
        let target = self.asc();
        let arg = ExprNode::Literal(value.into_filter_value());
        AggregateExpr::ordered_set(
            AggOp::HypotheticalCumeDist,
            arg,
            target,
            Some("DOUBLE PRECISION"),
        )
    }
}

// ── STRING_AGG ──────────────────────────────────────────────────────────────
// Gated on `V = String` — string concatenation is only meaningful on TEXT
// columns. The separator is user-supplied at call time and bound as a
// parameter (never interpolated into the SQL string) to guard against
// injection from a runtime-computed separator value.

impl<M: Model> FieldRef<M, String> {
    /// `STRING_AGG(column, sep)` — concatenates non-null string values with
    /// a separator, returned as `String`.
    /// The separator is bound as a positional parameter (`$N`) rather than
    /// interpolated directly into the SQL string, which means even a separator
    /// that contains SQL metacharacters is handled safely by the Postgres wire
    /// protocol.
    /// # Example
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
            within_group_order_by: Vec::new(),
        })
    }
}

// ── BOOL_AND / BOOL_OR ──────────────────────────────────────────────────────
// Gated on `V = bool` — boolean aggregates are only meaningful on BOOLEAN
// columns. Postgres emits NULL for an empty set; the typed surface returns
// `bool` which will be a runtime decode error on an empty grouping. Callers
// that need NULL-safe semantics wrap `Out` in `Option<bool>` themselves at
// the call site by using `ctx.raw_scalar` until a typed `Option<V>` decode
// path lands.

impl<M: Model> FieldRef<M, bool> {
    /// `BOOL_AND(column)` — returns `true` if every non-null value in the
    /// column is `true`, `false` if any non-null value is `false`.
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
    /// Same NULL behaviour as [`FieldRef::bool_and`] — empty groups produce
    /// NULL which decodes as a runtime error on the non-`Option` surface.
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn bool_or(self) -> AggregateExpr<bool> {
        AggregateExpr::unary_agg(AggOp::BoolOr, self.column(), None)
    }

    /// `EVERY(column)` — Postgres-standard alias for [`FieldRef::bool_and`].
    /// Returns `true` if every non-null value in the column is `true`,
    /// `false` if any non-null value is `false`.
    /// `EVERY` and `BOOL_AND` are interchangeable in Postgres — they
    /// produce identical results. Djogi exposes both because adopters
    /// reading from one set of docs expect the spelling they know;
    /// the emitter honours the user's choice (a call to `.every`
    /// emits `EVERY(col)`, not `BOOL_AND(col)`). Same NULL behaviour as
    /// [`FieldRef::bool_and`].
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn every(self) -> AggregateExpr<bool> {
        AggregateExpr::unary_agg(AggOp::Every, self.column(), None)
    }
}

// ── BIT_AND / BIT_OR / BIT_XOR ──────────────────────────────────────────────
// Bitwise integer aggregates. Sealed on a separate `IntegerColumn` trait
// rather than `Numeric` because Postgres BIT_AND / BIT_OR / BIT_XOR are
// defined for SMALLINT / INTEGER / BIGINT (and bit-string types Djogi
// doesn't model today) but NOT for floating-point types — `BIT_AND(REAL)`
// is a Postgres type error. Gating on `IntegerColumn` produces a compile-
// time error if a caller writes `f.score.bit_or` on an `f64` column,
// which beats the runtime Postgres error.
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
/// Implemented for `i16`, `i32`, `i64` — the three integer scalar types
/// Djogi blesses. Floating-point types (`f32`, `f64`), `time::Duration`,
/// and `Decimal` deliberately do NOT implement this; the corresponding
/// `BIT_AND(REAL)` / `BIT_OR(NUMERIC)` calls are Postgres type errors,
/// and gating at the type system catches them at compile time.
/// # Why a separate trait from [`super::arithmetic::Numeric`]?
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
    /// Useful for flag-bitmask reduction: `f.flags.bit_and` returns
    /// the set of bits set in *every* non-null row of the group.
    /// Identity (no rows or all NULL): all-bits-set in two's complement
    /// (`-1` for signed types). Empty groups decode as NULL — wrap `Out`
    /// in `Option<V>` at the call site (or use a `FILTER (WHERE ...)`
    /// guard) for NULL-safe handling.
    /// # Example
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
    /// Useful for "did any row have flag X set?" reductions:
    /// `f.flags.bit_or` returns the union of bits across the group.
    /// Identity: 0 (no bits set).
    /// # Postgres NULL behaviour
    /// Empty groups (or all-NULL inputs) decode as SQL NULL, which the
    /// non-`Option` typed surface treats as a runtime error. Wrap
    /// `Out = Option<V>` at the call site for NULL-safe handling, or
    /// chain `.filter(col.is_not_null)` so the aggregate sees a
    /// guaranteed-non-empty input.
    /// # Example
    /// ```ignore
    /// // Union of every set flag across the active session set
    /// let any_set: i32 = Session::objects()
    ///     .filter(|s| s.active().eq(true))
    ///     .aggregate(|s| s.flags().bit_or())
    ///     .fetch_one(&mut ctx).await?;
    /// ```
    #[must_use = "aggregates are lazy — dropping one silently omits the column"]
    pub fn bit_or(self) -> AggregateExpr<V> {
        AggregateExpr::unary_agg(AggOp::BitOr, self.column(), None)
    }

    /// `BIT_XOR(column)` — bitwise XOR across non-null integer values.
    /// Useful for parity / checksum-style aggregations. Postgres 14+
    /// adds this aggregate; Djogi's PostgreSQL 18 floor includes it.
    /// Identity: 0.
    /// # Postgres NULL behaviour
    /// Empty groups (or all-NULL inputs) decode as SQL NULL — same
    /// caveat as [`Self::bit_or`].
    /// # Example
    /// ```ignore
    /// // Parity bit across a row's per-event flag stream
    /// let parity: i64 = Event::objects()
    ///     .filter(|e| e.session_id().eq(session))
    ///     .aggregate(|e| e.checksum_part().bit_xor())
    ///     .fetch_one(&mut ctx).await?;
    /// ```
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
    use crate::query::portable::SqlEmitContext;

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
        emit_expr(&mut qb, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        let sql = qb.sql();
        assert_eq!(sql.trim(), "SUM(amount)", "got: {sql}");
    }

    #[test]
    fn emit_count_field() {
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = f.count();
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        let sql = qb.sql();
        assert_eq!(sql.trim(), "COUNT(amount)", "got: {sql}");
    }

    #[test]
    fn emit_count_star() {
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = f.count_star();
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut qb, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        let sql = qb.sql();
        assert_eq!(sql.trim(), "AVG(amount)", "got: {sql}");
    }

    #[test]
    fn emit_min_max_field() {
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &f.min().node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(qb.sql().trim(), "MIN(amount)");

        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &f.max().node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(qb.sql().trim(), "MAX(amount)");
    }

    #[test]
    fn emit_aggregate_with_filter() {
        // `f.amount.count.filter(f.amount.as_expr.lt(0))` must
        // emit `COUNT(amount) FILTER (WHERE amount < $1)`. One bind
        // for the literal 0 on the RHS; the column refs are bare.
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let g: FieldRef<Txn, i64> = FieldRef::new("amount");
        let cond = f.as_expr().lt(Expr::literal(0i64));
        let agg = g.count().filter(cond);
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut qb, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut qb, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        let sql = qb.sql();
        assert_eq!(sql.trim(), "ARRAY_AGG(tag)", "got: {sql}");
    }

    #[test]
    fn emit_json_agg() {
        let f: FieldRef<Txn, String> = FieldRef::new("tag");
        let agg = f.json_agg();
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        let sql = qb.sql();
        assert_eq!(sql.trim(), "JSONB_AGG(tag)", "got: {sql}");
    }

    #[test]
    fn emit_string_agg_binds_separator() {
        let f: FieldRef<Txn, String> = FieldRef::new("tag");
        let agg = f.string_agg(", ");
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        let sql = qb.sql();
        // Column is bare, separator is a bound parameter ($1).
        assert!(sql.contains("STRING_AGG(tag, $1)"), "got: {sql}");
    }

    #[test]
    fn emit_bool_and() {
        let f: FieldRef<Txn, bool> = FieldRef::new("active");
        let agg = f.bool_and();
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        let sql = qb.sql();
        assert_eq!(sql.trim(), "BOOL_AND(active)", "got: {sql}");
    }

    #[test]
    fn emit_bool_or() {
        let f: FieldRef<Txn, bool> = FieldRef::new("active");
        let agg = f.bool_or();
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        let sql = qb.sql();
        assert_eq!(sql.trim(), "BOOL_OR(active)", "got: {sql}");
    }

    // ── .distinct tests (T4) ────────────────────────────────────────────────

    #[test]
    fn sum_distinct_emits_sum_distinct() {
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = f.sum().distinct();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        // `count_star` returns `AggregateExpr<i64, ValueAgg>`, the
        // same kind as `count` / `sum` / `avg` — so the kind-state
        // does NOT reject `.distinct` on it (CountStar inherits the
        // value-aggregate modifier surface). The runtime check at
        // fetch time is the only guard for the COUNT(*)-specific
        // shape. We reach into the node directly (crate-private) to
        // construct the malformed aggregate that the check must catch.
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
        // ill-formed Postgres. With T1 the IR tracks per-aggregate
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

    // ── Fixup tests ──────────────────────────────────────────────────────

    #[test]
    fn count_star_with_order_by_rejected_at_fetch() {
        // COUNT(*) does not accept a per-aggregate ORDER BY — the emitter
        // hard-codes `COUNT(*)` and would silently drop the order_by slot.
        // Reject at fetch time so adopters see a typed error.
        let f: FieldRef<Txn, i64> = FieldRef::new("id");
        let agg = f.count_star().order_by(f.asc());
        let result = crate::expr::sql::check_aggregate_legality(&agg.node);
        assert!(
            result.is_err(),
            "expected UnsupportedAggregate error for COUNT(*) with ORDER BY"
        );
        let err = result.unwrap_err();
        assert!(
            matches!(err, crate::DjogiError::UnsupportedAggregate { .. }),
            "expected UnsupportedAggregate variant, got: {err:?}"
        );
    }

    #[test]
    fn grouping_bare_accepted_at_fetch() {
        // Bare GROUPING(col) without modifiers is the valid use case under
        // ROLLUP / CUBE / GROUPING SETS. Must pass legality check.
        let f: FieldRef<Txn, i64> = FieldRef::new("region_id");
        let agg = f.grouping();
        let result = crate::expr::sql::check_aggregate_legality(&agg.node);
        assert!(
            result.is_ok(),
            "bare GROUPING(col) should pass legality, got: {result:?}"
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        // an ORDER BY is present (T1).
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        crate::expr::sql::emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        let sql = acc.sql().to_string();
        assert!(
            sql.contains("ARRAY_AGG(DISTINCT id ORDER BY id ASC")
                && sql.contains(") FILTER (WHERE rank > "),
            "expected DISTINCT + ORDER BY + FILTER composition, got: {sql}"
        );
    }

    // ── .over(|w| ...) end-to-end tests ──────────────────────────────────────
    // These tests exercise the round-trip: `.over(|w| ...)` on `AggregateExpr`
    // stores a `WindowSpec` on the node, then `emit_aggregate_with_window_and_cast`
    // picks it up and emits the correct `OVER (...)` clause. The bare
    // `emit_expr` path (used for nested aggregates) does NOT emit the window
    // clause — window emission is handled exclusively at the terminal layer.

    #[test]
    fn over_empty_closure_stores_window_spec() {
        // `.over(|w| w)` sets `window: Some(WindowSpec::default)` — the
        // terminal layer will emit `OVER ` from it, preserving the pre-T3
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
        // `SUM(amount) OVER `. The narrowing cast (SUM_CAST) wraps the whole
        // expression in parens: `(SUM(amount) OVER )::BIGINT`.
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = f.sum().over(|w| w);
        let mut acc = SqlAccumulator::new("");
        crate::query::sql::emit_aggregate_with_window_and_cast(&mut acc, &agg.node)
            .expect("aggregate expression should lower to SQL");
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
        crate::query::sql::emit_aggregate_with_window_and_cast(&mut acc, &agg.node)
            .expect("aggregate expression should lower to SQL");
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
        crate::query::sql::emit_aggregate_with_window_and_cast(&mut acc, &agg.node)
            .expect("aggregate expression should lower to SQL");
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
        crate::query::sql::emit_aggregate_with_window_and_cast(&mut acc, &agg.node)
            .expect("aggregate expression should lower to SQL");
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
        crate::query::sql::emit_aggregate_with_window_and_cast(&mut acc, &agg.node)
            .expect("aggregate expression should lower to SQL");
        let sql = acc.sql().to_string();
        assert!(sql.contains("PARTITION BY dept_id"), "got: {sql}");
        assert!(!sql.contains("org_id"), "first spec should be gone: {sql}");
    }

    #[test]
    fn no_over_call_preserves_default_over_empty_via_terminal() {
        // When `.over(...)` is never called, `window: None` — the terminal
        // `emit_aggregate_with_window_and_cast` emits `OVER ` as before T3.
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = f.count();
        let mut acc = SqlAccumulator::new("");
        crate::query::sql::emit_aggregate_with_window_and_cast(&mut acc, &agg.node)
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "BIT_AND(flags)");
    }

    #[test]
    fn bit_or_emits_bit_or_keyword() {
        let f: FieldRef<Txn, i32> = FieldRef::new("flags");
        let agg = f.bit_or();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "BIT_OR(flags)");
    }

    #[test]
    fn bit_xor_emits_bit_xor_keyword() {
        let f: FieldRef<Txn, i64> = FieldRef::new("checksum_part");
        let agg = f.bit_xor();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "BIT_AND(DISTINCT flags)");
    }

    #[test]
    fn bit_aggregates_compose_with_filter() {
        // BIT_OR with FILTER attaches the predicate after the close paren.
        let f: FieldRef<Txn, i32> = FieldRef::new("flags");
        let g: FieldRef<Txn, i64> = FieldRef::new("active_at");
        let agg = f.bit_or().filter(g.as_expr().gt(Expr::literal(0i64)));
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "BIT_AND(DISTINCT flags ORDER BY flags ASC)");
    }

    // ── GROUPING (T10) ────────────────────────────────────────────────────

    #[test]
    fn grouping_emits_grouping_keyword() {
        // GROUPING(col) — single-column form. Bare emission matches
        // every other unary aggregate's shape because internally it
        // routes through emit_unary_agg.
        let f: FieldRef<Txn, String> = FieldRef::new("region");
        let agg = f.grouping();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "GROUPING(region)");
    }

    #[test]
    fn grouping_returns_i32_aggregate() {
        // Compile-time pin: GROUPING returns i32 because Postgres
        // returns INTEGER (single-column form).
        let f: FieldRef<Txn, String> = FieldRef::new("region");
        let _: AggregateExpr<i32, MetadataAgg> = f.grouping();
    }

    #[test]
    fn grouping_works_on_any_column_type() {
        // GROUPING accepts any column type — it inspects only whether
        // the column was rolled up in the current row, not the column's
        // value. The typed surface has no V bound for the same reason.
        let f_str: FieldRef<Txn, String> = FieldRef::new("region");
        let f_i64: FieldRef<Txn, i64> = FieldRef::new("dept_id");
        let f_bool: FieldRef<Txn, bool> = FieldRef::new("is_active");
        let _: AggregateExpr<i32, MetadataAgg> = f_str.grouping();
        let _: AggregateExpr<i32, MetadataAgg> = f_i64.grouping();
        let _: AggregateExpr<i32, MetadataAgg> = f_bool.grouping();
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "GROUPING aggregate must not carry modifiers")]
    fn direct_ir_grouping_distinct_debug_asserts() {
        let f: FieldRef<Txn, String> = FieldRef::new("region");
        let mut agg = f.grouping();
        if let ExprNode::Aggregate {
            ref mut distinct, ..
        } = agg.node
        {
            *distinct = true;
        }

        let _ = crate::expr::sql::check_aggregate_legality(&agg.node);
    }

    // ── GROUPING variadic (#94) ───────────────────────────────────────────

    #[test]
    fn grouping_of_two_args_emits_comma_separated() {
        let agg = crate::grouping_of(&["region", "dept"]);
        // Inspect the IR shape.
        match &agg.node {
            crate::expr::node::ExprNode::GroupingVariadic { args } => {
                assert_eq!(args.len(), 2);
                for (a, expected) in args.iter().zip(["region", "dept"]) {
                    match a {
                        crate::expr::node::ExprNode::Field { column } => {
                            assert_eq!(*column, expected)
                        }
                        _ => panic!("expected Field, got {a:?}"),
                    }
                }
            }
            other => panic!("expected GroupingVariadic, got {other:?}"),
        }
        // Emit and check SQL shape.
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root()).expect("emit");
        assert_eq!(acc.sql(), "GROUPING(region, dept)");
    }

    #[test]
    fn grouping_of_three_args_emits_comma_separated() {
        let agg = crate::grouping_of(&["region", "dept", "product"]);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root()).expect("emit");
        assert_eq!(acc.sql(), "GROUPING(region, dept, product)");
    }

    #[test]
    #[should_panic(expected = "GROUPING() with no args")]
    fn grouping_of_empty_panics() {
        let _ = crate::grouping_of(&[]);
    }

    #[test]
    fn grouping_of_legality_accepted() {
        // No modifiers applied — GroupingVariadic node is trivially legal.
        let agg = crate::grouping_of(&["region", "dept"]);
        assert!(crate::expr::sql::check_aggregate_legality(&agg.node).is_ok());
    }

    // ── JSON object aggregates (T9) ───────────────────────────────────────

    #[test]
    fn json_object_agg_emits_json_object_agg_key_value() {
        let f_key: FieldRef<Txn, i64> = FieldRef::new("id");
        let f_val: FieldRef<Txn, String> = FieldRef::new("name");
        let agg = f_key.json_object_agg(f_val);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "JSON_OBJECT_AGG(id, name)");
    }

    #[test]
    fn jsonb_object_agg_emits_jsonb_object_agg_key_value() {
        let f_key: FieldRef<Txn, i64> = FieldRef::new("id");
        let f_val: FieldRef<Txn, String> = FieldRef::new("name");
        let agg = f_key.jsonb_object_agg(f_val);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "JSONB_OBJECT_AGG(id, name)");
    }

    #[test]
    fn json_object_agg_key_first_value_second() {
        // The receiver is always the key, the argument is always the
        // value. Pinning this means a future API refactor that swapped
        // the two would fail the test.
        let f_key: FieldRef<Txn, String> = FieldRef::new("k");
        let f_val: FieldRef<Txn, i64> = FieldRef::new("v");
        let agg = f_key.jsonb_object_agg(f_val);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "JSONB_OBJECT_AGG(k, v)");
    }

    #[test]
    fn json_object_agg_alias_pair_emit_distinct_keywords() {
        // `json_object_agg` and `jsonb_object_agg` are *different* AggOp
        // variants that map to *different* Postgres functions (json vs
        // jsonb). The emitter must keep them distinct in the SQL token
        // stream (no collapse).
        let f_k1: FieldRef<Txn, i64> = FieldRef::new("k");
        let f_v1: FieldRef<Txn, String> = FieldRef::new("v");
        let f_k2: FieldRef<Txn, i64> = FieldRef::new("k");
        let f_v2: FieldRef<Txn, String> = FieldRef::new("v");
        let mut acc1 = SqlAccumulator::new("");
        let mut acc2 = SqlAccumulator::new("");
        emit_expr(
            &mut acc1,
            &f_k1.json_object_agg(f_v1).node,
            SqlEmitContext::root(),
        )
        .expect("aggregate expression should lower to SQL");
        emit_expr(
            &mut acc2,
            &f_k2.jsonb_object_agg(f_v2).node,
            SqlEmitContext::root(),
        )
        .expect("aggregate expression should lower to SQL");
        assert_eq!(acc1.sql(), "JSON_OBJECT_AGG(k, v)");
        assert_eq!(acc2.sql(), "JSONB_OBJECT_AGG(k, v)");
        assert_ne!(acc1.sql(), acc2.sql());
    }

    #[test]
    fn json_object_agg_returns_serde_value() {
        // Compile-time pin: both methods return
        // `AggregateExpr<serde_json::Value>` regardless of the input
        // key/value column types.
        let f_k: FieldRef<Txn, i64> = FieldRef::new("k");
        let f_v: FieldRef<Txn, String> = FieldRef::new("v");
        let _: AggregateExpr<serde_json::Value> = f_k.json_object_agg(f_v);
        let f_k2: FieldRef<Txn, String> = FieldRef::new("k");
        let f_v2: FieldRef<Txn, i64> = FieldRef::new("v");
        let _: AggregateExpr<serde_json::Value> = f_k2.jsonb_object_agg(f_v2);
    }

    // ── REGR_* family (T6) ────────────────────────────────────────────────

    #[test]
    fn regr_slope_emits_regr_slope_y_x() {
        let f_y: FieldRef<Txn, f64> = FieldRef::new("price");
        let f_x: FieldRef<Txn, f64> = FieldRef::new("hours");
        let agg = f_y.regr_slope(f_x);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "REGR_SLOPE(price, hours)");
    }

    #[test]
    fn regr_intercept_emits_regr_intercept_y_x() {
        let f_y: FieldRef<Txn, f64> = FieldRef::new("price");
        let f_x: FieldRef<Txn, f64> = FieldRef::new("hours");
        let agg = f_y.regr_intercept(f_x);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "REGR_INTERCEPT(price, hours)");
    }

    #[test]
    fn regr_r2_emits_regr_r2_y_x() {
        let f_y: FieldRef<Txn, f64> = FieldRef::new("price");
        let f_x: FieldRef<Txn, f64> = FieldRef::new("hours");
        let agg = f_y.regr_r2(f_x);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "REGR_R2(price, hours)");
    }

    #[test]
    fn regr_count_emits_regr_count_y_x() {
        let f_y: FieldRef<Txn, f64> = FieldRef::new("price");
        let f_x: FieldRef<Txn, f64> = FieldRef::new("hours");
        let agg = f_y.regr_count(f_x);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "REGR_COUNT(price, hours)");
    }

    #[test]
    fn regr_count_returns_i64_aggregate() {
        // Compile-time pin: REGR_COUNT is the only regression-family
        // aggregate that returns BIGINT (mirrors the unary `count`
        // shape) — every other regr_* method returns f64.
        let f_y: FieldRef<Txn, f64> = FieldRef::new("y");
        let f_x: FieldRef<Txn, f64> = FieldRef::new("x");
        let _: AggregateExpr<i64> = f_y.regr_count(f_x);
    }

    #[test]
    fn regr_avgx_avgy_emit_distinct_keywords() {
        // The two averages take y, x in that order regardless of which
        // variable is being averaged — Postgres convention.
        let f_y: FieldRef<Txn, f64> = FieldRef::new("y");
        let f_x: FieldRef<Txn, f64> = FieldRef::new("x");
        let mut acc1 = SqlAccumulator::new("");
        emit_expr(&mut acc1, &f_y.regr_avgx(f_x).node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc1.sql(), "REGR_AVGX(y, x)");

        let f_y2: FieldRef<Txn, f64> = FieldRef::new("y");
        let f_x2: FieldRef<Txn, f64> = FieldRef::new("x");
        let mut acc2 = SqlAccumulator::new("");
        emit_expr(
            &mut acc2,
            &f_y2.regr_avgy(f_x2).node,
            SqlEmitContext::root(),
        )
        .expect("aggregate expression should lower to SQL");
        assert_eq!(acc2.sql(), "REGR_AVGY(y, x)");
    }

    #[test]
    fn regr_sxx_sxy_syy_emit_distinct_keywords() {
        // Sum-of-squares family: SXX (x deviations²), SXY (xy
        // deviations), SYY (y deviations²). Each maps to its own
        // Postgres function; the emitter must pick the right keyword.
        let f_y: FieldRef<Txn, f64> = FieldRef::new("y");
        let f_x: FieldRef<Txn, f64> = FieldRef::new("x");

        let mut acc1 = SqlAccumulator::new("");
        emit_expr(&mut acc1, &f_y.regr_sxx(f_x).node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc1.sql(), "REGR_SXX(y, x)");

        let f_y2: FieldRef<Txn, f64> = FieldRef::new("y");
        let f_x2: FieldRef<Txn, f64> = FieldRef::new("x");
        let mut acc2 = SqlAccumulator::new("");
        emit_expr(&mut acc2, &f_y2.regr_sxy(f_x2).node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc2.sql(), "REGR_SXY(y, x)");

        let f_y3: FieldRef<Txn, f64> = FieldRef::new("y");
        let f_x3: FieldRef<Txn, f64> = FieldRef::new("x");
        let mut acc3 = SqlAccumulator::new("");
        emit_expr(&mut acc3, &f_y3.regr_syy(f_x3).node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc3.sql(), "REGR_SYY(y, x)");
    }

    #[test]
    fn regr_family_non_count_returns_f64() {
        // Compile-time pin: all regression-family methods except
        // `regr_count` return `AggregateExpr<f64>`. Mixed input numeric
        // types compose because both sides gate on `Numeric`
        // independently.
        let f_y: FieldRef<Txn, i64> = FieldRef::new("y");
        let f_x_i32: FieldRef<Txn, i32> = FieldRef::new("x");
        let f_x_f64: FieldRef<Txn, f64> = FieldRef::new("x");
        let _: AggregateExpr<f64> = f_y.regr_avgx(f_x_i32);
        let _: AggregateExpr<f64> = FieldRef::<Txn, i64>::new("y").regr_avgy(f_x_f64);
        let _: AggregateExpr<f64> =
            FieldRef::<Txn, f32>::new("y").regr_intercept(FieldRef::<Txn, f64>::new("x"));
        let _: AggregateExpr<f64> =
            FieldRef::<Txn, f32>::new("y").regr_r2(FieldRef::<Txn, f64>::new("x"));
        let _: AggregateExpr<f64> =
            FieldRef::<Txn, f32>::new("y").regr_slope(FieldRef::<Txn, f64>::new("x"));
        let _: AggregateExpr<f64> =
            FieldRef::<Txn, f32>::new("y").regr_sxx(FieldRef::<Txn, f64>::new("x"));
        let _: AggregateExpr<f64> =
            FieldRef::<Txn, f32>::new("y").regr_sxy(FieldRef::<Txn, f64>::new("x"));
        let _: AggregateExpr<f64> =
            FieldRef::<Txn, f32>::new("y").regr_syy(FieldRef::<Txn, f64>::new("x"));
    }

    // ── COVAR / CORR (T5 — binary aggregates) ─────────────────────────────

    #[test]
    fn covar_pop_emits_covar_pop_y_x() {
        // Self is y, arg is x. Postgres convention: y first.
        let f_y: FieldRef<Txn, f64> = FieldRef::new("revenue");
        let f_x: FieldRef<Txn, f64> = FieldRef::new("cost");
        let agg = f_y.covar_pop(f_x);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "COVAR_POP(revenue, cost)");
    }

    #[test]
    fn covar_samp_emits_covar_samp_y_x() {
        let f_y: FieldRef<Txn, i64> = FieldRef::new("revenue");
        let f_x: FieldRef<Txn, i64> = FieldRef::new("cost");
        let agg = f_y.covar_samp(f_x);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "COVAR_SAMP(revenue, cost)");
    }

    #[test]
    fn corr_emits_corr_y_x() {
        let f_y: FieldRef<Txn, f64> = FieldRef::new("score_a");
        let f_x: FieldRef<Txn, f64> = FieldRef::new("score_b");
        let agg = f_y.corr(f_x);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut acc, &agg_ab.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "COVAR_POP(a_col, b_col)");

        // b.covar(a) → COVAR(b, a) — receiver order is the SQL order.
        let f_a2: FieldRef<Txn, f64> = FieldRef::new("a_col");
        let f_b2: FieldRef<Txn, f64> = FieldRef::new("b_col");
        let agg_ba = f_b2.covar_pop(f_a2);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg_ba.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "STDDEV_POP(score)");
    }

    #[test]
    fn stddev_samp_emits_stddev_samp() {
        let f: FieldRef<Txn, f64> = FieldRef::new("score");
        let agg = f.stddev_samp();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "STDDEV_SAMP(score)");
    }

    #[test]
    fn stddev_alias_emits_bare_stddev() {
        // The `.stddev` alias preserves spelling — emits STDDEV, not
        // STDDEV_SAMP. Both names resolve to the same Postgres aggregate
        // semantically, but the emitter honours the caller's choice.
        let f: FieldRef<Txn, f64> = FieldRef::new("score");
        let agg = f.stddev();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "STDDEV(score)");
    }

    #[test]
    fn var_pop_emits_var_pop() {
        let f: FieldRef<Txn, i64> = FieldRef::new("score");
        let agg = f.var_pop();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "VAR_POP(score)");
    }

    #[test]
    fn var_samp_emits_var_samp() {
        let f: FieldRef<Txn, i64> = FieldRef::new("score");
        let agg = f.var_samp();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "VAR_SAMP(score)");
    }

    #[test]
    fn variance_alias_emits_bare_variance() {
        let f: FieldRef<Txn, i64> = FieldRef::new("score");
        let agg = f.variance();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut acc1, &f1.stddev_samp().node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        emit_expr(&mut acc2, &f2.stddev().node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut acc1, &f1.var_samp().node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        emit_expr(&mut acc2, &f2.variance().node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
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
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "EVERY(active)");
    }

    #[test]
    fn every_distinct_emits_every_distinct() {
        // EVERY composes with DISTINCT (semantic no-op on booleans, but
        // accepted by Postgres). The keyword stays `EVERY`.
        let f: FieldRef<Txn, bool> = FieldRef::new("active");
        let agg = f.every().distinct();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "EVERY(DISTINCT active)");
    }

    #[test]
    fn every_returns_bool_aggregate() {
        // Compile-time pin: `every` returns `AggregateExpr<bool>`,
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

    // ── PERCENTILE_CONT / PERCENTILE_DISC / MODE — T7 ordered-set ────────────

    #[test]
    fn percentile_cont_emits_within_group() {
        // Bare emission shape: PERCENTILE_CONT($1) WITHIN GROUP (ORDER BY ms ASC).
        let f: FieldRef<Txn, f64> = FieldRef::new("ms");
        let agg = f.percentile_cont(0.5);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        let sql = acc.sql().to_string();
        assert!(
            sql.starts_with("PERCENTILE_CONT($")
                && sql.contains(") WITHIN GROUP (ORDER BY ms ASC)"),
            "expected PERCENTILE_CONT($n) WITHIN GROUP (ORDER BY ms ASC), got: {sql}"
        );
    }

    #[test]
    fn percentile_disc_emits_within_group() {
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = f.percentile_disc(0.95);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        let sql = acc.sql().to_string();
        assert!(
            sql.starts_with("PERCENTILE_DISC($")
                && sql.contains(") WITHIN GROUP (ORDER BY amount ASC)"),
            "got: {sql}"
        );
    }

    #[test]
    fn mode_emits_within_group_no_args() {
        // MODE takes no function args — emits `MODE` with empty parens.
        let f: FieldRef<Txn, String> = FieldRef::new("category");
        let agg = f.mode();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        assert_eq!(acc.sql(), "MODE() WITHIN GROUP (ORDER BY category ASC)");
    }

    #[test]
    fn percentile_cont_returns_f64_regardless_of_column_type() {
        // PERCENTILE_CONT pins Out = f64 with DOUBLE PRECISION cast — same
        // approach as avg. Compile-time check via type ascription.
        let f_i16: FieldRef<Txn, i16> = FieldRef::new("a");
        let f_i64: FieldRef<Txn, i64> = FieldRef::new("b");
        let f_f64: FieldRef<Txn, f64> = FieldRef::new("c");
        let _: AggregateExpr<f64, OrderedSetAgg> = f_i16.percentile_cont(0.5);
        let _: AggregateExpr<f64, OrderedSetAgg> = f_i64.percentile_cont(0.5);
        let _: AggregateExpr<f64, OrderedSetAgg> = f_f64.percentile_cont(0.5);
    }

    #[test]
    fn percentile_disc_returns_column_type() {
        // PERCENTILE_DISC pins Out = V (the column type).
        let f_i64: FieldRef<Txn, i64> = FieldRef::new("amount");
        let f_str: FieldRef<Txn, String> = FieldRef::new("category");
        let _: AggregateExpr<i64, OrderedSetAgg> = f_i64.percentile_disc(0.5);
        let _: AggregateExpr<String, OrderedSetAgg> = f_str.percentile_disc(0.5);
    }

    #[test]
    fn mode_returns_column_type() {
        let f_i64: FieldRef<Txn, i64> = FieldRef::new("amount");
        let f_str: FieldRef<Txn, String> = FieldRef::new("category");
        let _: AggregateExpr<i64, OrderedSetAgg> = f_i64.mode();
        let _: AggregateExpr<String, OrderedSetAgg> = f_str.mode();
    }

    #[test]
    fn within_group_order_by_overrides_default_target() {
        // .within_group_order_by(other.desc) replaces the default ASC target
        // the typed builder set on construction. The replacement column must
        // be the same SQL/Rust decode type as the receiver (both f64 here) so
        // the aggregate's return-type contract is preserved — crossing types
        // (e.g. f64 receiver ordered by i64) would produce a runtime decode
        // failure and is explicitly disallowed by the public contract.
        let f: FieldRef<Txn, f64> = FieldRef::new("score");
        let other: FieldRef<Txn, f64> = FieldRef::new("response_time_ms");
        let agg = f.percentile_cont(0.95).within_group_order_by(other.desc());
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        let sql = acc.sql().to_string();
        assert!(
            sql.contains(") WITHIN GROUP (ORDER BY response_time_ms DESC)"),
            "expected DESC override on same-type compatible column, got: {sql}"
        );
    }

    #[test]
    fn mode_with_filter_accepted_at_fetch() {
        // FILTER (WHERE ...) is valid on ordered-set aggregates.
        let f: FieldRef<Txn, String> = FieldRef::new("category");
        let g: FieldRef<Txn, i64> = FieldRef::new("active");
        let agg = f.mode().filter(g.as_expr().gt(Expr::literal(0i64)));
        let result = crate::expr::sql::check_aggregate_legality(&agg.node);
        assert!(
            result.is_ok(),
            "MODE with FILTER should pass legality, got: {result:?}"
        );
    }

    #[test]
    fn percentile_cont_with_filter_emits_filter_after_within_group() {
        // FILTER attaches after the WITHIN GROUP clause — outside the
        // aggregate's outer expression. Same emission ordering as for
        // value aggregates.
        let f: FieldRef<Txn, f64> = FieldRef::new("ms");
        let g: FieldRef<Txn, i64> = FieldRef::new("active");
        let agg = f
            .percentile_cont(0.5)
            .filter(g.as_expr().gt(Expr::literal(0i64)));
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        let sql = acc.sql().to_string();
        assert!(
            sql.contains(") WITHIN GROUP (ORDER BY ms ASC) FILTER (WHERE active > "),
            "expected WITHIN GROUP then FILTER, got: {sql}"
        );
    }

    #[test]
    fn percentile_cont_within_group_target_pins_via_default_asc() {
        // Verify the constructor populates within_group_order_by with
        // the receiver column at default ASC — the typed builder
        // contract.
        let f: FieldRef<Txn, f64> = FieldRef::new("ms");
        let agg = f.percentile_cont(0.5);
        if let ExprNode::Aggregate {
            within_group_order_by,
            ..
        } = &agg.node
        {
            assert_eq!(within_group_order_by.len(), 1);
        } else {
            panic!("expected Aggregate node");
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "ordered-set / hypothetical-set aggregate must carry")]
    fn direct_ir_ordered_set_without_within_group_debug_asserts() {
        let f: FieldRef<Txn, f64> = FieldRef::new("ms");
        let mut agg = f.percentile_cont(0.5);
        if let ExprNode::Aggregate {
            within_group_order_by,
            ..
        } = &mut agg.node
        {
            within_group_order_by.clear();
        }

        let _ = crate::expr::sql::check_aggregate_legality(&agg.node);
    }

    // ── Hypothetical-set aggregates — T8 ─────────────────────────────────────

    #[test]
    fn rank_of_emits_within_group() {
        let f: FieldRef<Txn, i64> = FieldRef::new("salary");
        let agg = f.rank_of(7_500);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        let sql = acc.sql().to_string();
        assert!(
            sql.starts_with("RANK($") && sql.contains(") WITHIN GROUP (ORDER BY salary ASC)"),
            "got: {sql}"
        );
    }

    #[test]
    fn dense_rank_of_emits_within_group() {
        let f: FieldRef<Txn, i64> = FieldRef::new("salary");
        let agg = f.dense_rank_of(7_500);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        let sql = acc.sql().to_string();
        assert!(
            sql.starts_with("DENSE_RANK($") && sql.contains(") WITHIN GROUP (ORDER BY salary ASC)"),
            "got: {sql}"
        );
    }

    #[test]
    fn percent_rank_of_emits_within_group() {
        let f: FieldRef<Txn, f64> = FieldRef::new("ms");
        let agg = f.percent_rank_of(100.0);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        let sql = acc.sql().to_string();
        assert!(
            sql.starts_with("PERCENT_RANK($") && sql.contains(") WITHIN GROUP (ORDER BY ms ASC)"),
            "got: {sql}"
        );
    }

    #[test]
    fn cume_dist_of_emits_within_group() {
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let agg = f.cume_dist_of(500);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        let sql = acc.sql().to_string();
        assert!(
            sql.starts_with("CUME_DIST($") && sql.contains(") WITHIN GROUP (ORDER BY amount ASC)"),
            "got: {sql}"
        );
    }

    #[test]
    fn rank_of_returns_i64() {
        // Compile-time signature pin — RANK(value) returns BIGINT/i64.
        let f: FieldRef<Txn, i64> = FieldRef::new("salary");
        let _: AggregateExpr<i64, HypotheticalSetAgg> = f.rank_of(7_500);
        let _: AggregateExpr<i64, HypotheticalSetAgg> = f.dense_rank_of(7_500);
    }

    #[test]
    fn percent_rank_of_returns_f64() {
        // Compile-time signature pin — PERCENT_RANK / CUME_DIST return
        // DOUBLE PRECISION / f64.
        let f: FieldRef<Txn, i64> = FieldRef::new("amount");
        let _: AggregateExpr<f64, HypotheticalSetAgg> = f.percent_rank_of(500);
        let _: AggregateExpr<f64, HypotheticalSetAgg> = f.cume_dist_of(500);
    }

    #[test]
    fn hypothetical_set_with_filter_accepted_at_fetch() {
        let f: FieldRef<Txn, i64> = FieldRef::new("salary");
        let g: FieldRef<Txn, i64> = FieldRef::new("active");
        let agg = f.rank_of(7_500).filter(g.as_expr().gt(Expr::literal(0i64)));
        let result = crate::expr::sql::check_aggregate_legality(&agg.node);
        assert!(
            result.is_ok(),
            "FILTER must be accepted on hypothetical-set RANK, got: {result:?}"
        );
    }

    #[test]
    fn hypothetical_rank_within_group_override_works() {
        // The .within_group_order_by(...) modifier (T7) works for
        // hypothetical-set aggregates too — same IR slot.
        // Both the receiver (salary: i64), the supplied argument (7_500: i64),
        // and the replacement column (base_salary: i64) are the same type,
        // preserving the hypothetical-set comparability contract. Using a
        // replacement column of an incompatible type would violate the
        // contract and is explicitly disallowed by the public docs.
        let f: FieldRef<Txn, i64> = FieldRef::new("salary");
        let other: FieldRef<Txn, i64> = FieldRef::new("base_salary");
        let agg = f.rank_of(7_500).within_group_order_by(other.desc());
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &agg.node, SqlEmitContext::root())
            .expect("aggregate expression should lower to SQL");
        let sql = acc.sql().to_string();
        assert!(
            sql.contains(") WITHIN GROUP (ORDER BY base_salary DESC)"),
            "expected DESC override on different column, got: {sql}"
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "ordered-set / hypothetical-set aggregate must not carry")]
    fn direct_ir_hypothetical_set_order_by_debug_asserts() {
        let f: FieldRef<Txn, i64> = FieldRef::new("salary");
        let ordering: FieldRef<Txn, i64> = FieldRef::new("salary");
        let mut agg = f.rank_of(7_500);
        if let ExprNode::Aggregate { order_by, .. } = &mut agg.node {
            order_by.push(ordering.asc());
        }

        let _ = crate::expr::sql::check_aggregate_legality(&agg.node);
    }
}
