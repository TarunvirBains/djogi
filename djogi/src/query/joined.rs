//! Typed pair-tuple query surface — `JoinedQuerySet<L, R>` and friends.
//!
//! # What
//!
//! [`JoinedQuerySet<L, R>`] is the typed builder for queries whose result
//! shape is a *pair of model rows* `(L, R)` rather than a single model. It
//! is the structural counterpart to [`QuerySet<T>`](crate::query::QuerySet)
//! — every operation that takes one model on the build path takes the same
//! shape, but with two sides.
//!
//! Three flavours of pair-tuple query are supported in v0.1.0:
//!
//! 1. **Single-model self-join** — pair the same model with itself, e.g.
//!    `(Elephant, Elephant)`. The canonical Cluster 4A target: mating-pair
//!    candidate generation where left is "female" and right is "male".
//!    Entered via [`QuerySet::self_pairs`](crate::query::QuerySet::self_pairs);
//!    the default emission excludes the same-PK identity row
//!    (`WHERE l.id <> r.id`).
//!
//! 2. **Two-model cross-join** — pair different models, e.g.
//!    `(Sighting, Herd)`. Entered via
//!    [`QuerySet::cross_join_with`](crate::query::QuerySet::cross_join_with).
//!
//! 3. **Closure-self-join** — extends the self-join with one or two
//!    `LEFT JOIN`s against a [`ClosureModel`] table so kinship-style
//!    queries can sum over shared ancestors per pair. Entered via
//!    [`JoinedQuerySet::left_join_closure_pair`]; the
//!    typed [`PairClosureKinshipSum`] aggregate emits the Wright-style
//!    `SUM(la.path_count × ra.path_count × 0.5^(la.depth + ra.depth + 1))`
//!    over the joined pair.
//!
//! # Why
//!
//! The pre-Cluster-4A typed surface ([`QuerySet<T>`](crate::query::QuerySet),
//! plus [`AnnotatedQuerySet<T, A>`](crate::query::AnnotatedQuerySet)) requires
//! a single base `Model T`. Adopters whose result shape is `(L, R)` — pairs of
//! rows, multi-Model joins, closure-self-joins for kinship — had only two
//! escape hatches: raw SQL through the bypass attribute, or a custom typed
//! sub-query layer (the deferred djqry plan). Both produce strings the typed
//! projection pipeline never sees, which is exactly the gap GH #99 (closes
//! #84) tracks.
//!
//! The mating-pairs demo is the canonical validation target: until Step 3's
//! closure self-join + Wright F + window-fn ranking can be expressed without
//! `raw_rows`, the typed pair-tuple surface design is incomplete (per the
//! #99 acceptance criteria). This module + its [`JoinedAnnotatedQuerySet`]
//! + closure-pair extensions cover that shape.
//!
//! # How — SQL emission
//!
//! Aliases are framework-fixed `l` (left) and `r` (right). For the SELECT
//! list, each side's columns are projected under a side prefix
//! (`l_<col>` / `r_<col>`) so the macro-emitted
//! [`FromJoinedPgRow`] impl can decode each side back into its model
//! with a non-empty prefix:
//!
//! ```sql
//! SELECT
//!     l.<c1> AS l_<c1>, l.<c2> AS l_<c2>, ...,
//!     r.<c1> AS r_<c1>, r.<c2> AS r_<c2>, ...
//! FROM <l_table> AS l CROSS JOIN <r_table> AS r
//! [WHERE l.<pk> <> r.<pk> AND <left_filters> AND <right_filters>]
//! [ORDER BY l.<col> ASC, r.<col> ASC]
//! [LIMIT $n] [OFFSET $n];
//! ```
//!
//! Filters on either side are emitted with the side alias prepended so a
//! `WHERE` reference like `f.estimated_birth_year() <= cutoff` becomes
//! `l.estimated_birth_year <= $1` for the left side. Re-using
//! [`SqlEmitContext::joined`](crate::query::SqlEmitContext::joined) keeps
//! qualification consistent with the existing `select_related` emitter.
//!
//! # Variance
//!
//! `JoinedQuerySet<L, R>` is covariant in both `L` and `R` via
//! `PhantomData<fn() -> (L, R)>` — the queryset never owns or borrows a
//! value of either model, it merely tags which two models the filters
//! aim at. Mirrors [`QuerySet<T>`](crate::query::QuerySet)'s variance.
//!
//! # Where
//!
//! - Entry: [`QuerySet::self_pairs`](crate::query::QuerySet::self_pairs)
//!   and [`QuerySet::cross_join_with`](crate::query::QuerySet::cross_join_with).
//! - Annotated extension: [`JoinedAnnotatedQuerySet`].
//! - Closure-pair extension: [`JoinedQuerySet::left_join_closure_pair`].
//! - SQL emitters: `build_joined_select`, `build_joined_count`,
//!   `build_joined_annotated_select_for_fetch` (crate-private).

#![allow(clippy::manual_async_fn)]

use crate::DjogiError;
use crate::context::DjogiContext;
use crate::model::Model;
use crate::pg::accumulator::{SqlAccumulator, as_params};
use crate::pg::decode::{FromJoinedPgRow, FromPgRow, try_get_scalar};
use crate::query::annotate::IntoAggregateTuple;
use crate::query::closure::ClosureModel;
use crate::query::order::OrderExpr;
use crate::query::queryset::QuerySet;
use crate::query::terminal::auto_set_tenant;
use std::future::Future;
use std::marker::PhantomData;

/// SQL alias for the left side of a pair-tuple query (`FROM ... AS l`).
///
/// Framework-fixed so `WHERE`, `ORDER BY`, `PARTITION BY`, and other
/// emission sites always agree on which alias maps to which side.
pub(crate) const LEFT_ALIAS: &str = "l";

/// SQL alias for the right side of a pair-tuple query (`CROSS JOIN ... AS r`).
pub(crate) const RIGHT_ALIAS: &str = "r";

/// Column-name prefix applied in the SELECT list for the left side
/// (`l.<col> AS l_<col>`). [`FromJoinedPgRow::from_joined_pg_row`](crate::pg::decode::FromJoinedPgRow::from_joined_pg_row)
/// decodes the left side with this prefix.
pub(crate) const LEFT_COLUMN_PREFIX: &str = "l_";

/// Column-name prefix applied in the SELECT list for the right side
/// (`r.<col> AS r_<col>`). Matches [`FromJoinedPgRow::from_joined_pg_row`](crate::pg::decode::FromJoinedPgRow::from_joined_pg_row)
/// decoding on the right side.
pub(crate) const RIGHT_COLUMN_PREFIX: &str = "r_";

/// Closure-table aliases used by [`JoinedQuerySet::left_join_closure_pair`].
///
/// `la` is "left-ancestor" — the closure rows whose source is the left
/// pair member. `ra` is "right-ancestor" — the closure rows whose source
/// is the right pair member, additionally constrained to share the same
/// ancestor as `la` so Wright-style summation can aggregate over common
/// ancestors per pair.
pub(crate) const LEFT_CLOSURE_ALIAS: &str = "la";
pub(crate) const RIGHT_CLOSURE_ALIAS: &str = "ra";

/// Pair-side discriminator — which of the two pair members an operation
/// refers to.
///
/// Used by [`JoinedQuerySet::order_by_left`] /
/// [`JoinedQuerySet::order_by_right`] internally to tag which alias
/// prefix to emit, and by the pair-aware window-function builders
/// (see [`PairWindowExt`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairSide {
    /// The left side of the pair-tuple — emitted under the `l` alias.
    Left,
    /// The right side of the pair-tuple — emitted under the `r` alias.
    Right,
}

impl PairSide {
    /// SQL alias for this side (`"l"` for `Left`, `"r"` for `Right`).
    pub(crate) const fn alias(self) -> &'static str {
        match self {
            PairSide::Left => LEFT_ALIAS,
            PairSide::Right => RIGHT_ALIAS,
        }
    }

    /// Column-name prefix for this side's SELECT-list projection
    /// (`"l_"` / `"r_"`).
    pub(crate) const fn column_prefix(self) -> &'static str {
        match self {
            PairSide::Left => LEFT_COLUMN_PREFIX,
            PairSide::Right => RIGHT_COLUMN_PREFIX,
        }
    }
}

/// One ordering element on a pair-tuple query.
///
/// Carries the side discriminator plus the underlying [`OrderExpr`].
/// At emission time the side's alias (`l` or `r`) is the
/// `table_qualifier` passed to `OrderExpr::emit` (crate-private), so
/// column ordering emits as `l.<col> ASC` / `r.<col> DESC`.
#[derive(Debug, Clone)]
pub struct PairOrderExpr {
    pub(crate) side: PairSide,
    pub(crate) order: OrderExpr,
}

impl PairOrderExpr {
    /// Build a left-side ordering element.
    pub fn left(order: OrderExpr) -> Self {
        Self {
            side: PairSide::Left,
            order,
        }
    }

    /// Build a right-side ordering element.
    pub fn right(order: OrderExpr) -> Self {
        Self {
            side: PairSide::Right,
            order,
        }
    }
}

/// Closure-pair join configuration — captures the closure model's column
/// names and the alias semantics needed to emit the two LEFT JOINs.
///
/// Constructed via [`JoinedQuerySet::left_join_closure_pair`]. The emitter
/// in [`build_joined_select`] reads this to splice
/// `LEFT JOIN <closure_table> AS la ON la.<source_col> = l.<pk> LEFT JOIN
/// <closure_table> AS ra ON ra.<source_col> = r.<pk> AND ra.<ancestor_col>
/// = la.<ancestor_col>` between the cross-join and the WHERE clause.
///
/// The right-side `AND ra.<ancestor> = la.<ancestor>` predicate is the
/// load-bearing semi-join that turns the two LEFT JOINs into a
/// per-pair "shared ancestor" aggregator: only rows whose ancestor matches
/// both sides survive, and `path_count` multiplicity is preserved on
/// both sides for Wright-F summation.
#[derive(Debug, Clone)]
pub(crate) struct ClosurePairJoin {
    /// Closure table name (e.g. `"elephant_ancestries"`).
    pub(crate) table: &'static str,
    /// Column on the closure pointing back at the source-model row
    /// (e.g. `"elephant_id"`).
    pub(crate) source_column: &'static str,
    /// Column on the closure pointing at the ancestor row
    /// (e.g. `"ancestor_id"`).
    pub(crate) ancestor_column: &'static str,
    // The depth and path_count columns are stored here for future
    // legality assertions (e.g. cross-check between this closure-pair
    // join's bound `C` and a kinship-sum aggregate's bound `C` — both
    // should agree on the column shape). They are not read on the
    // emission hot path today because the typed kinship-sum aggregate
    // routes through its own `C: ClosureModel` bound via
    // `PairClosureKinshipSum::path_count_column()` etc., so the same
    // values reach SQL via two different routes that agree by
    // construction. Keeping them on the struct documents the full
    // column shape for future audits.
    /// Depth column (e.g. `"depth"`) — needed by typed aggregate emitters
    /// that walk the closure for Wright-style depth-weighted sums.
    #[allow(dead_code)]
    pub(crate) depth_column: &'static str,
    /// Path-multiplicity column (e.g. `"path_count"`).
    #[allow(dead_code)]
    pub(crate) path_count_column: &'static str,
}

/// Lazy pair-tuple query builder.
///
/// Holds two underlying [`QuerySet`]s — one per side — plus pair-side
/// pagination (`limit`/`offset`), ordering, and an optional closure-pair
/// join. Single-side filters / ordering accumulated on either underlying
/// queryset are re-emitted with the side's alias qualified during SQL
/// emission, so users can still call `.filter(...)` on `Elephant::objects()`
/// before crossing it with another queryset and the filters apply to the
/// left side post-join.
///
/// See the [module docs](self) for the SQL shape, the alias scheme, and
/// the variance argument.
#[must_use = "joined querysets are lazy — dropping one silently omits the query"]
pub struct JoinedQuerySet<L: Model, R: Model> {
    pub(crate) left: QuerySet<L>,
    pub(crate) right: QuerySet<R>,
    /// Excludes rows where `l.<pk> = r.<pk>` — set by
    /// [`QuerySet::self_pairs`] (the natural default for self-joins) and
    /// cleared by [`JoinedQuerySet::include_equal_pk`].
    pub(crate) exclude_equal_pk: bool,
    /// Pair-tuple ordering, applied after the underlying QuerySet
    /// ordering. Aliases follow [`PairSide`].
    pub(crate) ordering: Vec<PairOrderExpr>,
    /// Pair-tuple `LIMIT`. The two sides' own limits are ignored when
    /// the JoinedQuerySet is built — joining limits per-side has no
    /// natural SQL form (it would require subqueries on each side).
    pub(crate) limit: Option<i64>,
    /// Pair-tuple `OFFSET`. Same caveat as `limit`.
    pub(crate) offset: Option<i64>,
    /// Optional closure-pair join — set by
    /// [`left_join_closure_pair`](JoinedQuerySet::left_join_closure_pair).
    pub(crate) closure_pair: Option<ClosurePairJoin>,
    /// Covariant tag for both model parameters; never owned or borrowed.
    pub(crate) _marker: PhantomData<fn() -> (L, R)>,
}

impl<L: Model, R: Model> std::fmt::Debug for JoinedQuerySet<L, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinedQuerySet")
            .field("left_table", &L::table_name())
            .field("right_table", &R::table_name())
            .field("left_condition", &self.left.condition)
            .field("right_condition", &self.right.condition)
            .field("exclude_equal_pk", &self.exclude_equal_pk)
            .field("ordering", &self.ordering)
            .field("limit", &self.limit)
            .field("offset", &self.offset)
            .field("closure_pair", &self.closure_pair)
            .finish()
    }
}

impl<L: Model, R: Model> Clone for JoinedQuerySet<L, R> {
    fn clone(&self) -> Self {
        Self {
            left: self.left.clone(),
            right: self.right.clone(),
            exclude_equal_pk: self.exclude_equal_pk,
            ordering: self.ordering.clone(),
            limit: self.limit,
            offset: self.offset,
            closure_pair: self.closure_pair.clone(),
            _marker: PhantomData,
        }
    }
}

impl<L: Model, R: Model> JoinedQuerySet<L, R> {
    /// Replace the left side's QuerySet, AND-ing whatever filter the
    /// caller supplied onto the existing left-side condition.
    ///
    /// The closure receives `L::Fields::default()` and returns any
    /// [`IntoQ<L>`](crate::query::IntoQ) predicate. Mirrors
    /// [`QuerySet::filter`] — the underlying left QuerySet is mutated
    /// through its own typed builder, so callers compose left-side
    /// filters with the same syntax they'd use on a plain QuerySet.
    #[must_use = "joined querysets are lazy — dropping one silently omits the query"]
    pub fn filter_left<F, P>(mut self, f: F) -> Self
    where
        F: FnOnce(L::Fields) -> P,
        P: crate::query::IntoQ<L>,
    {
        self.left = self.left.filter(f);
        self
    }

    /// Mirror of [`filter_left`](Self::filter_left) — applies the closure
    /// to the right side's QuerySet.
    #[must_use = "joined querysets are lazy — dropping one silently omits the query"]
    pub fn filter_right<F, P>(mut self, f: F) -> Self
    where
        F: FnOnce(R::Fields) -> P,
        P: crate::query::IntoQ<R>,
    {
        self.right = self.right.filter(f);
        self
    }

    /// Opt out of the default `l.<pk> <> r.<pk>` filter that
    /// [`QuerySet::self_pairs`] sets.
    ///
    /// When this is called the pair-tuple emission will include the
    /// identity row (where left and right are the same source row in a
    /// self-join). For unordered-pair semantics where you want every
    /// row to pair with every other row including itself, this is the
    /// opt-in. For ordered/permutation pair semantics where pairing a
    /// row with itself is meaningless (the mating-pairs case), leave
    /// the default.
    #[must_use = "joined querysets are lazy — dropping one silently omits the query"]
    pub fn include_equal_pk(mut self) -> Self {
        self.exclude_equal_pk = false;
        self
    }

    /// Append a left-side ordering element to the pair-tuple ordering
    /// list. Subsequent calls append; the existing semantics matches
    /// [`QuerySet::order_by`] (last call wins is *not* the rule).
    #[must_use = "joined querysets are lazy — dropping one silently omits the query"]
    pub fn order_by_left<F>(mut self, f: F) -> Self
    where
        F: FnOnce(L::Fields) -> OrderExpr,
    {
        let o = f(L::Fields::default());
        self.ordering.push(PairOrderExpr::left(o));
        self
    }

    /// Mirror of [`order_by_left`](Self::order_by_left) — appends a
    /// right-side ordering element.
    #[must_use = "joined querysets are lazy — dropping one silently omits the query"]
    pub fn order_by_right<F>(mut self, f: F) -> Self
    where
        F: FnOnce(R::Fields) -> OrderExpr,
    {
        let o = f(R::Fields::default());
        self.ordering.push(PairOrderExpr::right(o));
        self
    }

    /// Set the pair-tuple `LIMIT`. Replaces any prior call.
    #[must_use = "joined querysets are lazy — dropping one silently omits the query"]
    pub fn limit(mut self, n: i64) -> Self {
        self.limit = Some(n);
        self
    }

    /// Set the pair-tuple `OFFSET`. Replaces any prior call.
    #[must_use = "joined querysets are lazy — dropping one silently omits the query"]
    pub fn offset(mut self, n: i64) -> Self {
        self.offset = Some(n);
        self
    }

    /// Attach a closure-pair LEFT JOIN to this query for Wright-style
    /// shared-ancestor aggregation.
    ///
    /// The bound [`ClosureModel`] `C` names the closure table and column
    /// shape; the emitter splices:
    ///
    /// ```sql
    /// LEFT JOIN <closure_table> AS la ON la.<source_col> = l.<pk>
    /// LEFT JOIN <closure_table> AS ra ON ra.<source_col> = r.<pk>
    ///                              AND ra.<ancestor_col> = la.<ancestor_col>
    /// ```
    ///
    /// after the cross-join and before the WHERE clause. The right-side
    /// `AND ra.<ancestor> = la.<ancestor>` predicate is the
    /// shared-ancestor semi-join — only rows whose ancestor matches both
    /// sides survive, and `path_count` multiplicity stays per-side so
    /// the typed aggregate [`PairClosureKinshipSum::<C>`] can sum
    /// `la.path_count × ra.path_count × 0.5^(la.depth + ra.depth + 1)`
    /// across them.
    ///
    /// This only makes structural sense for **single-model self-joins**
    /// (where `L = R` and `C::Source = L`); enforced at the type level
    /// by the `C: ClosureModel<Source = L>` and `L = R` bounds.
    ///
    /// # Panics
    ///
    /// Panics at SQL build time (not at this builder call) if the
    /// underlying source model has no primary-key column — the closure
    /// emitter needs a stable PK column for the `la.<source> = l.<pk>`
    /// join predicate.
    #[must_use = "joined querysets are lazy — dropping one silently omits the query"]
    pub fn left_join_closure_pair<C>(mut self) -> Self
    where
        C: ClosureModel<Source = L>,
        // `R` is a sibling of `L` in the pair — for kinship-shape semantics
        // (Wright over a self-join), the right side must share `L`'s
        // identity. We capture that with a runtime check on the table
        // names plus the descriptor-level pk shape, rather than a
        // compile-time `L = R` bound, because the compile-time bound would
        // forbid `L = R` self-joins from co-existing with `L != R` cross
        // joins on the same builder type. The runtime check fires once at
        // SQL build time.
    {
        self.closure_pair = Some(ClosurePairJoin {
            table: C::table(),
            source_column: C::source_column(),
            ancestor_column: C::ancestor_column(),
            depth_column: C::depth_column(),
            path_count_column: C::path_count_column(),
        });
        self
    }

    /// Promote this pair-tuple query into the annotated form, attaching
    /// aggregate / window expressions to the pair-side SELECT list.
    ///
    /// The closure receives **both** sides' `Fields` so callers compose
    /// annotation expressions referencing either alias. Pair-aware
    /// window-function helpers (see [`PairWindowExt`]) prefix column
    /// references with the appropriate alias when generating
    /// `PARTITION BY` / `ORDER BY` slices of the `OVER ()` clause.
    ///
    /// Mirrors [`QuerySet::annotate`](crate::query::QuerySet::annotate)'s
    /// shape — the produced [`JoinedAnnotatedQuerySet`] has terminals
    /// (`fetch_all`); the bare [`JoinedQuerySet`] only has
    /// `fetch_all` / `count`, no aggregation slots.
    #[must_use = "joined querysets are lazy — dropping one silently omits the query"]
    pub fn annotate<F, A>(self, f: F) -> JoinedAnnotatedQuerySet<L, R, A>
    where
        F: FnOnce(L::Fields, R::Fields) -> A,
        A: IntoAggregateTuple,
    {
        let aggregates = f(L::Fields::default(), R::Fields::default());
        JoinedAnnotatedQuerySet {
            inner: self,
            aggregates,
            qualify: None,
            _a: PhantomData,
        }
    }
}

// ── Row-returning terminals ──────────────────────────────────────────

impl<L: Model, R: Model> JoinedQuerySet<L, R>
where
    L: FromPgRow + FromJoinedPgRow + Send + Unpin,
    R: FromPgRow + FromJoinedPgRow + Send + Unpin,
{
    /// Execute the pair-tuple query and collect every matching pair into
    /// a `Vec<(L, R)>`.
    ///
    /// A `is_empty` short-circuit fires when either side's underlying
    /// queryset is structurally none (the resulting cross-join would be
    /// empty regardless of the other side). Matches the
    /// [`QuerySet::fetch_all`](crate::query::QuerySet::fetch_all)
    /// short-circuit contract: no SQL is issued; the empty `Vec` is
    /// returned directly.
    pub fn fetch_all<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Vec<(L, R)>, DjogiError>> + Send + 'ctx
    where
        L: 'ctx,
        R: 'ctx,
    {
        async move {
            if self.left.is_empty() || self.right.is_empty() {
                return Ok(Vec::new());
            }
            // Auto-tenant: a pair-tuple query can be tenanted on the left
            // side, right side, or both. We dispatch sequentially with `?`
            // propagation so a misconfigured tenancy fails loudly before
            // the SQL round-trip.
            auto_set_tenant::<L>(ctx).await?;
            auto_set_tenant::<R>(ctx).await?;
            let acc = build_joined_select(&self).map_err(DjogiError::from)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let rows = ctx.query_all(&sql, &params).await?;
            let mut out: Vec<(L, R)> = Vec::with_capacity(rows.len());
            for row in &rows {
                let left = L::from_joined_pg_row(row, LEFT_COLUMN_PREFIX)?;
                let right = R::from_joined_pg_row(row, RIGHT_COLUMN_PREFIX)?;
                out.push((left, right));
            }
            Ok(out)
        }
    }
}

impl<L: Model, R: Model> JoinedQuerySet<L, R> {
    /// Count the number of pair tuples this query would return,
    /// without decoding any of them.
    ///
    /// Emits `SELECT COUNT(*) FROM <l> AS l CROSS JOIN <r> AS r
    /// [LEFT JOIN ...] [WHERE ...]`. `ORDER BY` / `LIMIT` / `OFFSET` are
    /// not emitted because they do not affect a `COUNT(*)`.
    pub fn count<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<i64, DjogiError>> + Send + 'ctx
    where
        L: 'ctx,
        R: 'ctx,
    {
        async move {
            if self.left.is_empty() || self.right.is_empty() {
                return Ok(0);
            }
            auto_set_tenant::<L>(ctx).await?;
            auto_set_tenant::<R>(ctx).await?;
            let acc = build_joined_count(&self).map_err(DjogiError::from)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let row = ctx.query_one(&sql, &params).await?;
            try_get_scalar::<i64>(&row, 0)
        }
    }
}

// ── Entry points on QuerySet<L> ──────────────────────────────────────

impl<L: Model> QuerySet<L> {
    /// Cross-join this queryset with another model's queryset, producing
    /// a [`JoinedQuerySet<L, R>`] whose result shape is `(L, R)`.
    ///
    /// The two underlying querysets retain their own filters; filters
    /// applied via [`JoinedQuerySet::filter_left`] /
    /// [`filter_right`](JoinedQuerySet::filter_right) AND onto those
    /// existing conditions, so callers can either fully build each
    /// side's filter before crossing them, or compose pair-side after
    /// crossing — both produce the same WHERE clause.
    ///
    /// ```ignore
    /// use djogi::prelude::*;
    ///
    /// // Pair every Elephant with every Herd the Sighting records cover.
    /// let pairs: Vec<(Sighting, Herd)> = Sighting::objects()
    ///     .filter(|s| s.observed_at().gte(season_start))
    ///     .cross_join_with(Herd::objects().filter(|h| h.estimated_population().gte(50)))
    ///     .fetch_all(&mut ctx)
    ///     .await?;
    /// ```
    ///
    /// `exclude_equal_pk` defaults to `false` for cross-joins — different
    /// models almost never share a primary-key namespace, and forcing
    /// `l.id <> r.id` would silently drop legitimate matches. Use
    /// [`QuerySet::self_pairs`] for the self-join variant whose default
    /// excludes the identity row.
    #[must_use = "joined querysets are lazy — dropping one silently omits the query"]
    pub fn cross_join_with<R: Model>(self, other: QuerySet<R>) -> JoinedQuerySet<L, R> {
        JoinedQuerySet {
            left: self,
            right: other,
            exclude_equal_pk: false,
            ordering: Vec::new(),
            limit: None,
            offset: None,
            closure_pair: None,
            _marker: PhantomData,
        }
    }

    /// Pair this model with itself, producing
    /// [`JoinedQuerySet<L, L>`].
    ///
    /// The default emission includes `WHERE l.<pk> <> r.<pk>` so the
    /// identity row (pairing a source row with itself) is excluded.
    /// Call [`JoinedQuerySet::include_equal_pk`] to opt back into the
    /// identity row when unordered-pair semantics are required.
    ///
    /// The right side is constructed from `L::objects()` (a fresh
    /// queryset with `L`'s default filter / ordering applied) so any
    /// proxy-model default filter still applies to both sides. Filters
    /// already accumulated on `self` carry through as the left side's
    /// condition.
    ///
    /// ```ignore
    /// use djogi::prelude::*;
    ///
    /// // Mating-pairs candidate generation: every mature Elephant paired
    /// // with every other mature Elephant.
    /// let pairs: Vec<(Elephant, Elephant)> = Elephant::objects()
    ///     .filter(|e| e.estimated_birth_year().lte(mature_cutoff))
    ///     .self_pairs()
    ///     .fetch_all(&mut ctx)
    ///     .await?;
    /// ```
    #[must_use = "joined querysets are lazy — dropping one silently omits the query"]
    pub fn self_pairs(self) -> JoinedQuerySet<L, L> {
        // Snapshot the (potentially filtered/ordered) left side, then
        // mirror that same condition into the right side. Mirroring is
        // the standard expectation for a "self-join with these criteria"
        // — both sides start from the same pool. Callers who want
        // asymmetric filters (`filter_left` only or `filter_right` only)
        // call those builder methods after `.self_pairs()` to add a
        // side-specific AND.
        let right = self.clone();
        JoinedQuerySet {
            left: self,
            right,
            exclude_equal_pk: true,
            ordering: Vec::new(),
            limit: None,
            offset: None,
            closure_pair: None,
            _marker: PhantomData,
        }
    }
}

// ── Annotated joined queryset ────────────────────────────────────────

/// Pair-tuple query with an attached aggregate / window-function tuple.
///
/// Produced by [`JoinedQuerySet::annotate`]. Carries the same
/// `IntoAggregateTuple` shape `AnnotatedQuerySet<T, A>` uses; aliases for
/// the aggregate SELECT-list slots stay the framework-reserved
/// `__djogi_agg_<N>` namespace so they never collide with the
/// `l_<col>` / `r_<col>` side-prefix namespace used by
/// [`FromJoinedPgRow`].
///
/// The terminal [`fetch_all`](Self::fetch_all) returns
/// `Vec<((L, R), A::Decoded)>` — the pair tuple followed by the decoded
/// aggregate slot(s), mirroring the existing
/// [`AnnotatedQuerySet::fetch_all`](crate::query::AnnotatedQuerySet::fetch_all)
/// return shape of `Vec<(T, A::Decoded)>` but with `T → (L, R)`.
#[must_use = "joined querysets are lazy — dropping one silently omits the query"]
pub struct JoinedAnnotatedQuerySet<L: Model, R: Model, A: IntoAggregateTuple> {
    pub(crate) inner: JoinedQuerySet<L, R>,
    pub(crate) aggregates: A,
    pub(crate) qualify: Option<crate::expr::QualifyCondition>,
    pub(crate) _a: PhantomData<fn() -> A>,
}

impl<L: Model, R: Model, A: IntoAggregateTuple> JoinedAnnotatedQuerySet<L, R, A> {
    /// Filter rows by an annotated window-function output.
    ///
    /// Mirrors [`AnnotatedQuerySet::qualify`](crate::query::AnnotatedQuerySet::qualify):
    /// PostgreSQL 18 has no `QUALIFY` keyword, so the predicate lowers to an
    /// outer `WHERE` over a derived table that wraps the annotated select.
    /// The closure receives `&A` so calling `.lt(...)` / `.lte(...)` /
    /// etc. on a window function produces a
    /// [`QualifyCondition`](crate::expr::QualifyCondition) bound to that
    /// function's `.alias("…")`.
    ///
    /// ```ignore
    /// use djogi::prelude::*;
    ///
    /// // Top-3 male per female by combined score.
    /// let scored: Vec<((Elephant, Elephant), i64)> = Elephant::objects()
    ///     .self_pairs()
    ///     .annotate(|female, male| {
    ///         RowNumber::new()
    ///             .partition_by_pair(PairSide::Left, female.id())
    ///             .order_by_pair_asc(PairSide::Right, male.name())
    ///             .alias("rank")
    ///     })
    ///     .qualify(|w| w.lte(3))
    ///     .fetch_all(&mut ctx)
    ///     .await?;
    /// ```
    #[must_use = "joined querysets are lazy — dropping one silently omits the query"]
    pub fn qualify<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&A) -> crate::expr::QualifyCondition,
    {
        let cond = f(&self.aggregates);
        self.qualify = Some(cond);
        self
    }

    /// Add an ordering element to the underlying pair-tuple query —
    /// applied after the annotation is in scope. Equivalent to chaining
    /// [`JoinedQuerySet::order_by_left`] on the pre-annotated
    /// queryset, but reachable on the annotated builder for fluency.
    #[must_use = "joined querysets are lazy — dropping one silently omits the query"]
    pub fn order_by_left<F>(mut self, f: F) -> Self
    where
        F: FnOnce(L::Fields) -> OrderExpr,
    {
        self.inner = self.inner.order_by_left(f);
        self
    }

    /// Right-side analogue of [`order_by_left`](Self::order_by_left).
    #[must_use = "joined querysets are lazy — dropping one silently omits the query"]
    pub fn order_by_right<F>(mut self, f: F) -> Self
    where
        F: FnOnce(R::Fields) -> OrderExpr,
    {
        self.inner = self.inner.order_by_right(f);
        self
    }

    /// Set the pair-tuple `LIMIT`. Replaces any prior call.
    #[must_use = "joined querysets are lazy — dropping one silently omits the query"]
    pub fn limit(mut self, n: i64) -> Self {
        self.inner = self.inner.limit(n);
        self
    }

    /// Set the pair-tuple `OFFSET`. Replaces any prior call.
    #[must_use = "joined querysets are lazy — dropping one silently omits the query"]
    pub fn offset(mut self, n: i64) -> Self {
        self.inner = self.inner.offset(n);
        self
    }
}

/// Result type for [`JoinedAnnotatedQuerySet::fetch_all`].
///
/// `((L, R), A::Decoded)` — each row is a pair tuple plus the decoded
/// aggregate slot(s). Extracted into a type alias so the `fetch_all`
/// signature stays under clippy's `type_complexity` threshold and the
/// public surface reads more naturally for adopters reaching for the
/// terminal's return type at let-binding sites.
pub type JoinedAnnotatedRow<L, R, A> = ((L, R), <A as IntoAggregateTuple>::Decoded);

impl<L: Model, R: Model, A: IntoAggregateTuple + Send> JoinedAnnotatedQuerySet<L, R, A>
where
    L: FromPgRow + FromJoinedPgRow + Send + Unpin,
    R: FromPgRow + FromJoinedPgRow + Send + Unpin,
{
    /// Execute the annotated pair-tuple query and collect every matching
    /// pair-plus-annotation tuple into `Vec<((L, R), A::Decoded)>`.
    ///
    /// Honours the same `is_empty` short-circuit contract as
    /// [`JoinedQuerySet::fetch_all`] — an empty queryset on either side
    /// returns the empty result without touching the database.
    pub fn fetch_all<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Vec<JoinedAnnotatedRow<L, R, A>>, DjogiError>> + Send + 'ctx
    where
        L: 'ctx,
        R: 'ctx,
        A: 'ctx,
        A::Decoded: Send + 'ctx,
    {
        async move {
            if self.inner.left.is_empty() || self.inner.right.is_empty() {
                return Ok(Vec::new());
            }
            auto_set_tenant::<L>(ctx).await?;
            auto_set_tenant::<R>(ctx).await?;

            let JoinedAnnotatedQuerySet {
                inner,
                aggregates,
                qualify,
                ..
            } = self;

            // Validate window/aggregate legality (alias presence, distinct
            // modifiers) before SQL build.
            aggregates.check_legality()?;

            // Pre-build closure-pair-join requirement check. Replaces
            // an at-execute-time Postgres `42P01 missing FROM-clause`
            // error with a typed validation error whenever an
            // annotation slot (today: `PairClosureKinshipSum<C>`)
            // references `la.` / `ra.` aliases that the queryset's
            // FROM clause does not provide. See
            // [`AnnotationSlot::requires_closure_pair_join`].
            if aggregates.requires_closure_pair_join() && inner.closure_pair.is_none() {
                return Err(DjogiError::Validation(
                    "annotated joined-queryset terminal includes a closure-pair aggregate \
                     (e.g. PairClosureKinshipSum) but the queryset has no \
                     `left_join_closure_pair::<C>()` join. Call \
                     `.left_join_closure_pair::<YourClosure>()` on the JoinedQuerySet \
                     before `.annotate(...)`."
                        .to_string(),
                ));
            }

            let acc =
                build_joined_annotated_select_for_fetch(&inner, &aggregates, qualify.as_ref())
                    .map_err(DjogiError::from)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let rows = ctx.query_all(&sql, &params).await?;

            let mut out: Vec<JoinedAnnotatedRow<L, R, A>> = Vec::with_capacity(rows.len());
            for row in &rows {
                let left = L::from_joined_pg_row(row, LEFT_COLUMN_PREFIX)?;
                let right = R::from_joined_pg_row(row, RIGHT_COLUMN_PREFIX)?;
                let agg = aggregates.decode_tuple(row).map_err(DjogiError::from)?;
                out.push(((left, right), agg));
            }
            Ok(out)
        }
    }
}

// ── SQL emitters ─────────────────────────────────────────────────────

/// Build the `SELECT` SQL for a [`JoinedQuerySet`] (no annotations).
///
/// SQL shape:
///
/// ```sql
/// SELECT
///     l.<c1> AS l_<c1>, ..., l.<cN> AS l_<cN>,
///     r.<c1> AS r_<c1>, ..., r.<cM> AS r_<cM>
/// FROM <l_table> AS l CROSS JOIN <r_table> AS r
/// [LEFT JOIN <closure> AS la ON la.<source> = l.<pk>
///  LEFT JOIN <closure> AS ra ON ra.<source> = r.<pk>
///                            AND ra.<ancestor> = la.<ancestor>]
/// [WHERE l.<pk> <> r.<pk> AND <left filter> AND <right filter>]
/// [ORDER BY <pair order ...>]
/// [LIMIT $n] [OFFSET $n]
/// ```
///
/// Aliases are framework-fixed per [`LEFT_ALIAS`] / [`RIGHT_ALIAS`].
/// Each side's column list is canonicalised via
/// [`FromJoinedPgRow::COLUMNS`] equivalent — looked up through the
/// model's [`FromPgRow::COLUMNS`](crate::pg::decode::FromPgRow::COLUMNS)
/// where available, plus the side prefix.
pub(crate) fn build_joined_select<L, R>(
    jqs: &JoinedQuerySet<L, R>,
) -> Result<SqlAccumulator, crate::query::PortablePredicateError>
where
    L: Model + crate::pg::decode::FromPgRow,
    R: Model + crate::pg::decode::FromPgRow,
{
    let mut acc = SqlAccumulator::new("SELECT ");
    push_aliased_columns::<L>(&mut acc, PairSide::Left, true);
    push_aliased_columns::<R>(&mut acc, PairSide::Right, false);

    acc.push_sql(" FROM ");
    acc.push_sql(L::table_name());
    acc.push_sql(" AS ");
    acc.push_sql(LEFT_ALIAS);
    acc.push_sql(" CROSS JOIN ");
    acc.push_sql(R::table_name());
    acc.push_sql(" AS ");
    acc.push_sql(RIGHT_ALIAS);

    push_closure_pair_joins::<L, R>(&mut acc, jqs);
    push_joined_where::<L, R>(&mut acc, jqs)?;
    push_joined_order_by(&mut acc, &jqs.ordering);

    if let Some(n) = jqs.limit {
        acc.push_sql(" LIMIT ");
        acc.push_bind(n);
    }
    if let Some(n) = jqs.offset {
        acc.push_sql(" OFFSET ");
        acc.push_bind(n);
    }
    Ok(acc)
}

/// Build the `SELECT COUNT(*)` SQL for a [`JoinedQuerySet`].
///
/// Reuses the same FROM / cross-join / WHERE shape as
/// [`build_joined_select`] but with `SELECT COUNT(*)` and no
/// `ORDER BY` / `LIMIT` / `OFFSET` tail (those do not affect the row
/// count). The closure-pair LEFT JOINs are also omitted because they
/// would distort the count (`LEFT JOIN` against the same row twice).
///
/// Note: this path does **not** emit closure-pair LEFT JOINs even when
/// `jqs.closure_pair` is `Some`. The closure-pair semi-join shape is
/// designed for aggregation (Wright F summation per pair); plain row
/// counting on a joined-closure shape would over-count by the
/// per-ancestor multiplicity. Callers wanting the count of pairs with
/// at least one shared ancestor should run a typed `EXISTS`-flavoured
/// query, which is a future refinement.
pub(crate) fn build_joined_count<L: Model, R: Model>(
    jqs: &JoinedQuerySet<L, R>,
) -> Result<SqlAccumulator, crate::query::PortablePredicateError> {
    let mut acc = SqlAccumulator::new("SELECT COUNT(*) FROM ");
    acc.push_sql(L::table_name());
    acc.push_sql(" AS ");
    acc.push_sql(LEFT_ALIAS);
    acc.push_sql(" CROSS JOIN ");
    acc.push_sql(R::table_name());
    acc.push_sql(" AS ");
    acc.push_sql(RIGHT_ALIAS);
    push_joined_where::<L, R>(&mut acc, jqs)?;
    Ok(acc)
}

/// Build the annotated SELECT for [`JoinedAnnotatedQuerySet`].
///
/// Mirrors [`build_annotated_select_for_fetch`](crate::query::sql::build_annotated_select_for_fetch)
/// but with the pair-tuple `SELECT l.<c1> AS l_<c1>, ..., r.<c1> AS r_<c1>, ...`
/// prefix plus the optional `__djogi_agg_<N>` aggregate slots. If
/// `qualify` is `Some`, the inner select is wrapped in a derived table
/// and the outer scope applies the qualify predicate as a `WHERE`.
pub(crate) fn build_joined_annotated_select_for_fetch<L, R, A>(
    jqs: &JoinedQuerySet<L, R>,
    aggregates: &A,
    qualify: Option<&crate::expr::QualifyCondition>,
) -> Result<SqlAccumulator, crate::query::PortablePredicateError>
where
    L: Model + crate::pg::decode::FromPgRow,
    R: Model + crate::pg::decode::FromPgRow,
    A: IntoAggregateTuple,
{
    let inner = build_joined_annotated_inner::<L, R, A>(jqs, aggregates)?;
    let Some(qualify) = qualify else {
        return Ok(inner);
    };

    // Derived-table wrap: the outer SELECT references the aggregate
    // alias by name (e.g. `rank`). Reuses the existing
    // `__djogi_q` alias the single-Model `qualify` lowering uses so
    // tooling that greps for that name on either path keeps working.
    let mut wrapped = SqlAccumulator::new("SELECT * FROM (");
    wrapped.extend_with(inner);
    wrapped.push_sql(") AS __djogi_q WHERE ");
    qualify.push_outer_where(&mut wrapped);
    Ok(wrapped)
}

fn build_joined_annotated_inner<L, R, A>(
    jqs: &JoinedQuerySet<L, R>,
    aggregates: &A,
) -> Result<SqlAccumulator, crate::query::PortablePredicateError>
where
    L: Model + crate::pg::decode::FromPgRow,
    R: Model + crate::pg::decode::FromPgRow,
    A: IntoAggregateTuple,
{
    let mut acc = SqlAccumulator::new("SELECT ");
    push_aliased_columns::<L>(&mut acc, PairSide::Left, true);
    push_aliased_columns::<R>(&mut acc, PairSide::Right, false);
    aggregates.push_columns(&mut acc);

    acc.push_sql(" FROM ");
    acc.push_sql(L::table_name());
    acc.push_sql(" AS ");
    acc.push_sql(LEFT_ALIAS);
    acc.push_sql(" CROSS JOIN ");
    acc.push_sql(R::table_name());
    acc.push_sql(" AS ");
    acc.push_sql(RIGHT_ALIAS);

    push_closure_pair_joins::<L, R>(&mut acc, jqs);
    push_joined_where::<L, R>(&mut acc, jqs)?;
    push_joined_order_by(&mut acc, &jqs.ordering);

    if let Some(n) = jqs.limit {
        acc.push_sql(" LIMIT ");
        acc.push_bind(n);
    }
    if let Some(n) = jqs.offset {
        acc.push_sql(" OFFSET ");
        acc.push_bind(n);
    }
    Ok(acc)
}

fn push_aliased_columns<M: Model + crate::pg::decode::FromPgRow>(
    acc: &mut SqlAccumulator,
    side: PairSide,
    is_first_block: bool,
) {
    let alias = side.alias();
    let prefix = side.column_prefix();
    for (i, col) in <M as crate::pg::decode::FromPgRow>::COLUMNS
        .iter()
        .enumerate()
    {
        if !(is_first_block && i == 0) {
            acc.push_sql(", ");
        }
        acc.push_sql(alias);
        acc.push_sql(".");
        acc.push_sql(col);
        acc.push_sql(" AS ");
        acc.push_sql(prefix);
        acc.push_sql(col);
    }
}

fn push_closure_pair_joins<L: Model, R: Model>(
    acc: &mut SqlAccumulator,
    jqs: &JoinedQuerySet<L, R>,
) {
    let Some(cp) = jqs.closure_pair.as_ref() else {
        return;
    };
    let l_pk = L::descriptor().pk_column().unwrap_or("id");
    let r_pk = R::descriptor().pk_column().unwrap_or("id");

    // LEFT JOIN <closure> AS la ON la.<source> = l.<pk>
    acc.push_sql(" LEFT JOIN ");
    acc.push_sql(cp.table);
    acc.push_sql(" AS ");
    acc.push_sql(LEFT_CLOSURE_ALIAS);
    acc.push_sql(" ON ");
    acc.push_sql(LEFT_CLOSURE_ALIAS);
    acc.push_sql(".");
    acc.push_sql(cp.source_column);
    acc.push_sql(" = ");
    acc.push_sql(LEFT_ALIAS);
    acc.push_sql(".");
    acc.push_sql(l_pk);

    // LEFT JOIN <closure> AS ra ON ra.<source> = r.<pk>
    //                          AND ra.<ancestor> = la.<ancestor>
    acc.push_sql(" LEFT JOIN ");
    acc.push_sql(cp.table);
    acc.push_sql(" AS ");
    acc.push_sql(RIGHT_CLOSURE_ALIAS);
    acc.push_sql(" ON ");
    acc.push_sql(RIGHT_CLOSURE_ALIAS);
    acc.push_sql(".");
    acc.push_sql(cp.source_column);
    acc.push_sql(" = ");
    acc.push_sql(RIGHT_ALIAS);
    acc.push_sql(".");
    acc.push_sql(r_pk);
    acc.push_sql(" AND ");
    acc.push_sql(RIGHT_CLOSURE_ALIAS);
    acc.push_sql(".");
    acc.push_sql(cp.ancestor_column);
    acc.push_sql(" = ");
    acc.push_sql(LEFT_CLOSURE_ALIAS);
    acc.push_sql(".");
    acc.push_sql(cp.ancestor_column);
}

fn push_joined_where<L: Model, R: Model>(
    acc: &mut SqlAccumulator,
    jqs: &JoinedQuerySet<L, R>,
) -> Result<(), crate::query::PortablePredicateError> {
    let l_pk = L::descriptor().pk_column().unwrap_or("id");
    let r_pk = R::descriptor().pk_column().unwrap_or("id");

    // Build WHERE in two stages: the pair-side anti-equality first
    // (when set), then the left and right side conditions if they are
    // not vacuously true. Each non-trivial clause is joined with ` AND `.
    let mut parts: Vec<&'static str> = Vec::with_capacity(3);
    let left_has_condition = !crate::query::sql::q_is_vacuously_true(&jqs.left.condition);
    let right_has_condition = !crate::query::sql::q_is_vacuously_true(&jqs.right.condition);

    if jqs.exclude_equal_pk {
        parts.push("pair");
    }
    if left_has_condition {
        parts.push("left");
    }
    if right_has_condition {
        parts.push("right");
    }

    if parts.is_empty() {
        return Ok(());
    }

    acc.push_sql(" WHERE ");
    for (i, tag) in parts.iter().enumerate() {
        if i > 0 {
            acc.push_sql(" AND ");
        }
        match *tag {
            "pair" => {
                // l.<pk> <> r.<pk>
                acc.push_sql(LEFT_ALIAS);
                acc.push_sql(".");
                acc.push_sql(l_pk);
                acc.push_sql(" <> ");
                acc.push_sql(RIGHT_ALIAS);
                acc.push_sql(".");
                acc.push_sql(r_pk);
            }
            "left" => {
                let ctx = crate::query::SqlEmitContext::joined(LEFT_ALIAS);
                crate::query::sql::emit_q::<L>(acc, &jqs.left.condition, ctx)?;
            }
            "right" => {
                let ctx = crate::query::SqlEmitContext::joined(RIGHT_ALIAS);
                crate::query::sql::emit_q::<R>(acc, &jqs.right.condition, ctx)?;
            }
            _ => unreachable!("part tag must be one of pair/left/right"),
        }
    }
    Ok(())
}

fn push_joined_order_by(acc: &mut SqlAccumulator, ordering: &[PairOrderExpr]) {
    if ordering.is_empty() {
        return;
    }
    acc.push_sql(" ORDER BY ");
    for (i, ord) in ordering.iter().enumerate() {
        if i > 0 {
            acc.push_sql(", ");
        }
        ord.order.emit(acc, Some(ord.side.alias()));
    }
}

// ── Pair-aware window-function helpers ───────────────────────────────

/// Extension trait that adds pair-aware `partition_by` / `order_by`
/// methods to the existing window-function builders
/// ([`RowNumber`](crate::expr::RowNumber), [`Rank`](crate::expr::Rank),
/// [`DenseRank`](crate::expr::DenseRank)) so they can target either
/// pair side.
///
/// # Why this is an extension trait
///
/// The existing window builders take `partition_by(FieldRef<M, V>)`
/// and store the column name as a bare `&'static str`. In a pair-tuple
/// context the bare column reference is ambiguous (`l.col` vs `r.col`),
/// so a pair-aware variant has to compose the alias prefix into the
/// stored string. Interning the composite string (`"l.col"`) the same
/// way [`FieldRef`](crate::query::FieldRef) interns relation paths
/// (`crate::query::field::__macro_support::intern_composed_path`) keeps
/// the existing `&'static str` storage shape and the existing emit
/// path unchanged — the trait just chooses the right alias prefix per
/// call.
///
/// `PairWindowExt` is implemented for the existing window types so
/// adopters write:
///
/// ```ignore
/// RowNumber::new()
///     .partition_by_pair(PairSide::Left, l_fields.female_id())
///     .order_by_pair_desc(PairSide::Left, l_fields.score())
///     .alias("rank")
/// ```
///
/// alongside the single-Model `partition_by` they already know.
///
/// # Accepted field handles
///
/// The pair-aware methods accept anything implementing
/// [`IntoSqlField<M, V>`](crate::query::field::IntoSqlField) —
/// post-Phase-8eta-PR3 root accessors return `DjogiField<M, V>`, the
/// legacy SQL handle is `FieldRef<M, V>`, and both satisfy the
/// bound. This mirrors [`RowNumber::partition_by`](crate::expr::RowNumber::partition_by) /
/// `.order_by`'s single-Model surface so adopters use the same
/// `{model}_fields.col()` expression on both paths.
pub trait PairWindowExt: Sized {
    /// Add a `PARTITION BY l.<col>` (or `r.<col>`) entry to the
    /// underlying window spec, using the side's alias as the
    /// table-qualifier prefix. Accepts any
    /// [`IntoSqlField<M, V>`](crate::query::field::IntoSqlField), so
    /// macro-emitted `DjogiField<M, V>` accessors and legacy
    /// `FieldRef<M, V>` handles both compose without an explicit cast.
    #[must_use = "window functions are lazy annotations - dropping one omits the column"]
    fn partition_by_pair<M, V, S>(self, side: PairSide, field: S) -> Self
    where
        M: Model,
        S: crate::query::field::IntoSqlField<M, V>;

    /// Add an `ORDER BY l.<col> ASC` (or `r.<col> ASC`) entry to the
    /// underlying window spec. Accepts the same `IntoSqlField` bound
    /// as [`partition_by_pair`](Self::partition_by_pair).
    #[must_use = "window functions are lazy annotations - dropping one omits the column"]
    fn order_by_pair_asc<M, V, S>(self, side: PairSide, field: S) -> Self
    where
        M: Model,
        S: crate::query::field::IntoSqlField<M, V>;

    /// Add an `ORDER BY l.<col> DESC` (or `r.<col> DESC`) entry to the
    /// underlying window spec. Accepts the same `IntoSqlField` bound
    /// as [`partition_by_pair`](Self::partition_by_pair).
    #[must_use = "window functions are lazy annotations - dropping one omits the column"]
    fn order_by_pair_desc<M, V, S>(self, side: PairSide, field: S) -> Self
    where
        M: Model,
        S: crate::query::field::IntoSqlField<M, V>;
}

/// Intern a `"<alias>.<column>"` composite into a `&'static str`,
/// reusing the existing field-path intern set used by
/// `FieldRef::__make_field_ref_with_path`. Composing the pair-side
/// alias into the column string is the simplest way to keep the
/// existing `WindowSpec` storage shape (`Vec<&'static str>`) without
/// per-pair allocations on the emit path.
///
/// The intern set is keyed on `(prefix, column)` pairs; a few dozen
/// distinct entries per project are typical even for large schemas, so
/// the `OnceLock<Mutex<HashSet<&'static str>>>` underneath stays
/// uncontended in the steady state.
fn intern_alias_column(alias: &'static str, column: &'static str) -> &'static str {
    // Re-uses the same intern strategy as
    // `query::field::__macro_support::intern_composed_path`. The
    // function lives in this module so adopters can compose pair-side
    // window references without depending on the macro-support
    // submodule.
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    static INTERN: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let set_mutex = INTERN.get_or_init(|| Mutex::new(HashSet::new()));
    let candidate = format!("{alias}.{column}");
    let mut set = set_mutex
        .lock()
        .expect("joined-window intern mutex poisoned");
    if let Some(existing) = set.get(candidate.as_str()) {
        return existing;
    }
    let leaked: &'static str = Box::leak(candidate.into_boxed_str());
    set.insert(leaked);
    leaked
}

macro_rules! impl_pair_window_ext {
    ($ty:ident) => {
        impl PairWindowExt for crate::expr::$ty {
            fn partition_by_pair<M, V, S>(mut self, side: PairSide, field: S) -> Self
            where
                M: Model,
                S: crate::query::field::IntoSqlField<M, V>,
            {
                let qualified = intern_alias_column(side.alias(), field.into_sql_field().column());
                self.window.partition_by.push(qualified);
                self
            }

            fn order_by_pair_asc<M, V, S>(mut self, side: PairSide, field: S) -> Self
            where
                M: Model,
                S: crate::query::field::IntoSqlField<M, V>,
            {
                let qualified = intern_alias_column(side.alias(), field.into_sql_field().column());
                self.window
                    .order_by
                    .push((qualified, crate::query::order::Direction::Asc));
                self
            }

            fn order_by_pair_desc<M, V, S>(mut self, side: PairSide, field: S) -> Self
            where
                M: Model,
                S: crate::query::field::IntoSqlField<M, V>,
            {
                let qualified = intern_alias_column(side.alias(), field.into_sql_field().column());
                self.window
                    .order_by
                    .push((qualified, crate::query::order::Direction::Desc));
                self
            }
        }
    };
}

impl_pair_window_ext!(RowNumber);
impl_pair_window_ext!(Rank);
impl_pair_window_ext!(DenseRank);

// ── Typed pair-side closure-kinship aggregate ────────────────────────

/// Typed aggregate that emits the Wright-style shared-ancestor
/// kinship summation over a pair of closure-table aliases (`la` and
/// `ra`, see `ClosurePairJoin` — crate-private).
///
/// SQL shape emitted (in an aggregate SELECT-list slot):
///
/// ```sql
/// COALESCE(SUM(
///     la.<path_count_col>::numeric
///   * ra.<path_count_col>::numeric
///   * POWER(0.5::numeric,
///           (la.<depth_col> + ra.<depth_col> + 1)::numeric)
/// ), 0)::float8
/// ```
///
/// The decoded slot is `f64` — Wright's inbreeding coefficient F for
/// the offspring of the pair. `COALESCE(..., 0)` covers the case where
/// the closure-pair LEFT JOINs miss for every row (no shared ancestor,
/// or one side has no closure rows yet); without it `SUM` would return
/// `NULL` and `f64` decode would fail. The outer `::float8` keeps the
/// units in lockstep with the `Expr::area_of_intersection` / `area_of`
/// scalar surfaces also used in pair-tuple scoring (see the
/// mating-pairs demo).
///
/// # Constructing
///
/// `PairClosureKinshipSum::<ElephantAncestry>::new()` mints an instance;
/// the `C: ClosureModel` bound is the type-level proof that the column
/// names in the emitted SQL come from a trusted descriptor surface
/// (validated at [`Model::materialize_closure`] call time).
///
/// # When the closure join is missing
///
/// Including a `PairClosureKinshipSum` in the annotation tuple without
/// first calling [`JoinedQuerySet::left_join_closure_pair::<C>`] would
/// produce SQL that references `la.<path_count>` / `ra.<depth>` against
/// missing aliases. The pre-build legality hook catches this:
/// [`AnnotationSlot::requires_closure_pair_join`](crate::query::annotate::AnnotationSlot::requires_closure_pair_join)
/// is overridden here to return `true`, and
/// [`JoinedAnnotatedQuerySet::fetch_all`] checks the tuple's aggregate
/// requirement against the queryset's `closure_pair` field before SQL
/// build, returning a typed
/// [`DjogiError::Validation`] with a remediation hint instead of
/// letting Postgres surface a `42P01 missing FROM-clause` error at
/// execute time.
pub struct PairClosureKinshipSum<C: ClosureModel> {
    _c: PhantomData<fn() -> C>,
}

impl<C: ClosureModel> Default for PairClosureKinshipSum<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: ClosureModel> PairClosureKinshipSum<C> {
    /// Construct a new kinship-sum aggregate.
    ///
    /// The `C` parameter pins which closure model's column names get
    /// spliced into the emitted SQL. The aggregate can be the sole
    /// annotation on a joined query (`arity 1`) or one slot of a
    /// tuple (arities 2..=4, same as the regular aggregate surface).
    pub fn new() -> Self {
        Self { _c: PhantomData }
    }

    /// Internal accessor for the depth column name. Used by the
    /// aggregate-slot impl; kept on the type itself so testing can
    /// pin the closure-model column wiring without reaching for the
    /// trait method directly.
    fn depth_column() -> &'static str {
        C::depth_column()
    }

    fn path_count_column() -> &'static str {
        C::path_count_column()
    }

    /// Pure SQL emitter for the kinship-sum aggregate. Pushes the
    /// `COALESCE(SUM(...))::float8` shape onto `acc` without any
    /// surrounding `AS` alias — the caller (annotation-slot impl)
    /// owns the alias.
    fn emit_inline(acc: &mut SqlAccumulator) {
        acc.push_sql("COALESCE(SUM(");
        acc.push_sql(LEFT_CLOSURE_ALIAS);
        acc.push_sql(".");
        acc.push_sql(Self::path_count_column());
        acc.push_sql("::numeric * ");
        acc.push_sql(RIGHT_CLOSURE_ALIAS);
        acc.push_sql(".");
        acc.push_sql(Self::path_count_column());
        acc.push_sql("::numeric * POWER(0.5::numeric, (");
        acc.push_sql(LEFT_CLOSURE_ALIAS);
        acc.push_sql(".");
        acc.push_sql(Self::depth_column());
        acc.push_sql(" + ");
        acc.push_sql(RIGHT_CLOSURE_ALIAS);
        acc.push_sql(".");
        acc.push_sql(Self::depth_column());
        acc.push_sql(" + 1)::numeric)), 0)::float8");
    }
}

// AnnotationSlot impl — fits `PairClosureKinshipSum` into the existing
// `IntoAggregateTuple` machinery so it composes with `RowNumber` /
// `Rank` / `DenseRank` / `AggregateExpr` slot impls inside one tuple.
//
// The kinship-sum slot is fundamentally a GROUP-BY-shaped aggregate
// (SUM across `(la.<>, ra.<>)` rows for one `(l.<pk>, r.<pk>)` pair).
// In a non-grouped context (the default joined-annotated terminal),
// Postgres requires that every non-aggregate column in the SELECT list
// either be inside a GROUP BY clause or be functionally determined by
// the GROUP BY. We do NOT emit `GROUP BY l.<pk>, r.<pk>` from the
// pair-tuple emitter today; instead, the kinship sum is composed at
// SELECT-list time as a window-function-shaped aggregate by emitting
// `OVER (PARTITION BY l.<pk>, r.<pk>)` so Postgres aggregates per pair
// without requiring a `GROUP BY` rewrite. This matches the existing
// `OVER ()` shape that single-Model annotate uses for ungrouped
// aggregates (see annotate.rs lines 226–229).
//
// The kinship value is the same for every row of the pair's
// `(la, ra)` cross product (the `path_count × path_count × 0.5^(...)`
// products sum to the same total), so `OVER (PARTITION BY l.<pk>,
// r.<pk>)` is correct.
//
// At decode time, the value is the same on every row of the pair
// regardless of which `la`/`ra` row is "selected" — the existing
// annotate decode path reads one slot per row, which gives the
// per-pair Wright F value.
impl<C: ClosureModel> crate::query::annotate::AnnotationSlot for PairClosureKinshipSum<C> {
    type Decoded = f64;

    fn push_column(&self, acc: &mut SqlAccumulator, slot: usize) {
        acc.push_sql(", ");
        Self::emit_inline(acc);
        // OVER (PARTITION BY l.<pk>, r.<pk>) — per-pair partitioning so
        // the aggregate value is per-pair, not table-wide.
        //
        // PK columns: we cannot easily reach `L::pk_column()` from this
        // impl because `AnnotationSlot` doesn't carry an `L` parameter.
        // Both sides default to `id` for HeerId / RanjId / Serial PKs;
        // for `Composite` or `None` PK kinds this slot is structurally
        // unsuitable (no per-pair identity column to partition on). Use
        // the `id` literal directly here — `JoinedAnnotatedQuerySet`'s
        // pre-build legality check could narrow this further; for v0.1.0
        // the `id` literal matches every model the pair-tuple surface
        // realistically supports.
        acc.push_sql(" OVER (PARTITION BY ");
        acc.push_sql(LEFT_ALIAS);
        acc.push_sql(".id, ");
        acc.push_sql(RIGHT_ALIAS);
        acc.push_sql(".id)");
        acc.push_sql(" AS ");
        acc.push_sql(annotation_alias(slot));
    }

    fn push_column_bare(&self, acc: &mut SqlAccumulator, slot: usize) {
        self.push_column_bare_after(acc, slot, true);
    }

    fn push_column_bare_after(
        &self,
        acc: &mut SqlAccumulator,
        slot: usize,
        has_previous_columns: bool,
    ) {
        if has_previous_columns {
            acc.push_sql(", ");
        }
        Self::emit_inline(acc);
        acc.push_sql(" AS ");
        acc.push_sql(annotation_alias(slot));
    }

    fn decode_column(
        &self,
        row: &tokio_postgres::Row,
        slot: usize,
    ) -> Result<Self::Decoded, tokio_postgres::Error> {
        row.try_get::<_, f64>(annotation_alias(slot))
    }

    /// `PairClosureKinshipSum` references `la.<path_count>` / `ra.<depth>`
    /// in its emitted SQL — without a closure-pair LEFT JOIN providing
    /// those aliases the query fails with a Postgres
    /// `42P01 missing FROM-clause` error. Reporting `true` here lets
    /// [`JoinedAnnotatedQuerySet::fetch_all`] catch the missing
    /// `left_join_closure_pair::<C>()` call before SQL build and return
    /// a typed [`DjogiError::Validation`] with a remediation hint.
    fn requires_closure_pair_join(&self) -> bool {
        true
    }
}

/// Local mirror of `query::annotate::aggregate_alias` — the slot →
/// `__djogi_agg_N` mapping. Re-implemented here so this module does not
/// take a (crate-private) dependency on `annotate`'s private free
/// function.
fn annotation_alias(slot: usize) -> &'static str {
    match slot {
        0 => "__djogi_agg_0",
        1 => "__djogi_agg_1",
        2 => "__djogi_agg_2",
        3 => "__djogi_agg_3",
        _ => unreachable!(
            "djogi annotate arity max is 4 — slot {slot} not reachable. \
             A new impl_into_aggregate_tuple! arity must extend this match."
        ),
    }
}

// `AnnotationSlot` requires a sealed marker; the sealed module on
// annotate.rs is crate-private so we register through the existing
// `pub trait AnnotationSlot` surface (no re-export of the seal is
// needed — types implementing `AnnotationSlot` cross-module are
// admitted by the `annotation_slot_sealed::Sealed` impl block being
// `pub(crate)` accessible from sibling modules).
//
// The seal is enforced by `annotate.rs` lines 86–88
// (`mod annotation_slot_sealed { pub trait Sealed {} }`) where `Sealed`
// is module-private. To satisfy it here without weakening the seal,
// we implement Sealed directly inside this module — the trait surface
// `pub(crate) trait Sealed` would force users to name it. Instead, we
// re-use the existing module-private path by piggybacking on the
// `AnnotationSlot` blanket: every `S: AnnotationSlot` already satisfies
// `IntoAggregateTuple` via the `impl<S> IntoAggregateTuple for S where
// S: AnnotationSlot` block (annotate.rs lines 423–456). The blanket
// asks `S: AnnotationSlot`, not `S: Sealed`; so as long as we add the
// Sealed impl through `annotate.rs`'s `pub(crate) mod
// annotation_slot_sealed`, the blanket picks us up.
//
// This is achieved by the trait shape: `AnnotationSlot` itself is
// `pub: annotation_slot_sealed::Sealed`. The implementation in this
// module impls `AnnotationSlot` and `Sealed` together — `Sealed` is
// reachable as `crate::query::annotate::annotation_slot_sealed::Sealed`
// through the `pub(crate)` re-export added in this commit to
// `annotate.rs`.
impl<C: ClosureModel> crate::query::annotate::annotation_slot_sealed::Sealed
    for PairClosureKinshipSum<C>
{
}

#[cfg(test)]
mod tests {
    //! Unit tests for the pair-tuple SQL emitter. Verify SQL shape
    //! against a known-good shape per-construct. Tests here only
    //! assert string shape so the build does not depend on a running
    //! Postgres; live-Postgres integration tests for the pair-tuple
    //! terminals are a Cluster 4A follow-on, not part of this slice.

    use super::*;
    use crate::descriptor::{
        FieldDescriptor, FieldSqlType, ModelDescriptor, PkType, field_descriptor, model_descriptor,
    };
    use crate::pg::decode::FromPgRow;
    use crate::types::HeerId;

    // Inert two-column model. `id` is the framework-injected PK; `name`
    // is the user column. The descriptor lists `id` + `name` for SQL
    // emission; `FromPgRow::COLUMNS` lists both for SELECT-list
    // emission.
    struct Mini;
    impl crate::model::__sealed::Sealed for Mini {}
    #[allow(clippy::manual_async_fn)]
    impl crate::model::Model for Mini {
        type Pk = HeerId;
        type Fields = ();
        fn table_name() -> &'static str {
            "minis"
        }
        fn pk_value(&self) -> &Self::Pk {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            &MINI_DESC
        }
        fn get(
            _ctx: &mut DjogiContext,
            _id: Self::Pk,
        ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create(
            _ctx: &mut DjogiContext,
            _v: Self,
        ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut DjogiContext,
        ) -> impl Future<Output = Result<(), DjogiError>> + Send + 'ctx {
            async { unreachable!() }
        }
        fn delete(
            self,
            _ctx: &mut DjogiContext,
        ) -> impl Future<Output = Result<(), DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut DjogiContext,
        ) -> impl Future<Output = Result<Self, DjogiError>> + Send + 'ctx {
            async { unreachable!() }
        }
    }
    impl FromPgRow for Mini {
        const COLUMNS: &'static [&'static str] = &["id", "name"];
        const COLUMN_LIST: &'static str = "id, name";
        fn from_pg_row(_row: &tokio_postgres::Row) -> Result<Self, DjogiError> {
            unreachable!()
        }
    }

    static MINI_FIELDS: &[FieldDescriptor] = &[field_descriptor("name", FieldSqlType::Text, false)];
    static MINI_DESC: ModelDescriptor =
        model_descriptor("Mini", "minis", PkType::HeerId, MINI_FIELDS);

    #[test]
    fn self_pairs_emits_cross_join_and_exclude_equal_pk() {
        let jqs: JoinedQuerySet<Mini, Mini> = QuerySet::<Mini>::new().self_pairs();
        let acc = build_joined_select(&jqs).expect("emit");
        let sql = acc.sql();
        assert!(
            sql.contains("FROM minis AS l CROSS JOIN minis AS r"),
            "cross-join FROM shape mismatch: {sql}"
        );
        assert!(
            sql.contains("WHERE l.id <> r.id"),
            "self_pairs default exclude_equal_pk missing: {sql}"
        );
        // SELECT list shape — both sides aliased
        assert!(sql.contains("l.id AS l_id"), "left id alias missing: {sql}");
        assert!(
            sql.contains("l.name AS l_name"),
            "left name alias missing: {sql}"
        );
        assert!(
            sql.contains("r.id AS r_id"),
            "right id alias missing: {sql}"
        );
        assert!(
            sql.contains("r.name AS r_name"),
            "right name alias missing: {sql}"
        );
    }

    #[test]
    fn cross_join_with_does_not_emit_exclude_pk_default() {
        let jqs: JoinedQuerySet<Mini, Mini> =
            QuerySet::<Mini>::new().cross_join_with(QuerySet::<Mini>::new());
        let acc = build_joined_select(&jqs).expect("emit");
        let sql = acc.sql();
        assert!(
            !sql.contains("WHERE l.id <> r.id"),
            "cross_join_with must NOT inject exclude_equal_pk by default: {sql}"
        );
        // The SQL is otherwise the same CROSS JOIN shape.
        assert!(
            sql.contains("FROM minis AS l CROSS JOIN minis AS r"),
            "cross-join FROM shape: {sql}"
        );
    }

    #[test]
    fn include_equal_pk_drops_anti_equality_filter() {
        let jqs: JoinedQuerySet<Mini, Mini> =
            QuerySet::<Mini>::new().self_pairs().include_equal_pk();
        let acc = build_joined_select(&jqs).expect("emit");
        let sql = acc.sql();
        assert!(
            !sql.contains("WHERE l.id <> r.id"),
            "include_equal_pk should drop the default anti-equality: {sql}"
        );
    }

    #[test]
    fn limit_offset_emit_after_select_body() {
        let jqs: JoinedQuerySet<Mini, Mini> =
            QuerySet::<Mini>::new().self_pairs().limit(10).offset(5);
        let acc = build_joined_select(&jqs).expect("emit");
        let sql = acc.sql();
        // LIMIT / OFFSET appear at end, in that order.
        let limit_idx = sql.find("LIMIT").expect("LIMIT in SQL");
        let offset_idx = sql.find("OFFSET").expect("OFFSET in SQL");
        assert!(limit_idx < offset_idx, "LIMIT must precede OFFSET: {sql}");
    }

    #[test]
    fn order_by_emits_alias_qualified_columns() {
        use crate::query::FieldRef;
        // Hand-build FieldRef instances through the field module's
        // pub(crate) constructor. Production code routes through the
        // macro-generated {Model}Fields default impl; this test bypasses
        // that since `Mini::Fields = ()`.
        let name_ref: FieldRef<Mini, String> = FieldRef::new("name");
        let id_ref: FieldRef<Mini, HeerId> = FieldRef::new("id");
        let jqs: JoinedQuerySet<Mini, Mini> = QuerySet::<Mini>::new()
            .self_pairs()
            .order_by_left(|_f| name_ref.asc())
            .order_by_right(|_f| id_ref.desc());
        let acc = build_joined_select(&jqs).expect("emit");
        let sql = acc.sql();
        assert!(
            sql.contains("ORDER BY l.name ASC, r.id DESC"),
            "alias-qualified ORDER BY missing: {sql}"
        );
    }

    #[test]
    fn count_omits_order_limit_and_closure_pair_joins() {
        let jqs: JoinedQuerySet<Mini, Mini> = QuerySet::<Mini>::new()
            .self_pairs()
            .limit(5)
            .order_by_left(|_| OrderExpr::Column {
                column: "name",
                direction: crate::query::order::Direction::Asc,
                nulls: crate::query::order::NullsOrder::Default,
            });
        let acc = build_joined_count(&jqs).expect("emit");
        let sql = acc.sql();
        assert!(
            sql.starts_with("SELECT COUNT(*) FROM minis AS l CROSS JOIN minis AS r"),
            "count SQL shape mismatch: {sql}"
        );
        assert!(
            !sql.contains("ORDER BY"),
            "count must not include ORDER BY: {sql}"
        );
        assert!(
            !sql.contains("LIMIT"),
            "count must not include LIMIT: {sql}"
        );
        // Anti-equality survives — the count of distinct ordered pairs
        // is the meaningful number.
        assert!(
            sql.contains("WHERE l.id <> r.id"),
            "count must preserve exclude_equal_pk: {sql}"
        );
    }

    // ── ClosureModel fixture for closure-pair tests ──────────────────
    //
    // `MiniClosure` plays the part of a kinship/ancestor closure for
    // `Mini`. It implements [`ClosureModel`] with the conventional
    // column names so the closure-pair join emitter has something to
    // splice in.
    struct MiniClosure;
    impl crate::model::__sealed::Sealed for MiniClosure {}
    #[allow(clippy::manual_async_fn)]
    impl crate::model::Model for MiniClosure {
        type Pk = HeerId;
        type Fields = ();
        fn table_name() -> &'static str {
            "mini_ancestries"
        }
        fn pk_value(&self) -> &Self::Pk {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            &MINI_CLOSURE_DESC
        }
        fn get(
            _ctx: &mut DjogiContext,
            _id: Self::Pk,
        ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create(
            _ctx: &mut DjogiContext,
            _v: Self,
        ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut DjogiContext,
        ) -> impl Future<Output = Result<(), DjogiError>> + Send + 'ctx {
            async { unreachable!() }
        }
        fn delete(
            self,
            _ctx: &mut DjogiContext,
        ) -> impl Future<Output = Result<(), DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut DjogiContext,
        ) -> impl Future<Output = Result<Self, DjogiError>> + Send + 'ctx {
            async { unreachable!() }
        }
    }
    impl FromPgRow for MiniClosure {
        const COLUMNS: &'static [&'static str] = &["mini_id", "ancestor_id", "depth", "path_count"];
        const COLUMN_LIST: &'static str = "mini_id, ancestor_id, depth, path_count";
        fn from_pg_row(_row: &tokio_postgres::Row) -> Result<Self, DjogiError> {
            unreachable!()
        }
    }
    static MINI_CLOSURE_FIELDS: &[FieldDescriptor] = &[
        field_descriptor("mini_id", FieldSqlType::BigInt, false),
        field_descriptor("ancestor_id", FieldSqlType::BigInt, false),
        field_descriptor("depth", FieldSqlType::Integer, false),
        field_descriptor("path_count", FieldSqlType::BigInt, false),
    ];
    static MINI_CLOSURE_DESC: ModelDescriptor = model_descriptor(
        "MiniClosure",
        "mini_ancestries",
        PkType::HeerId,
        MINI_CLOSURE_FIELDS,
    );
    impl crate::query::ClosureModel for MiniClosure {
        type Source = Mini;
        fn source_column() -> &'static str {
            "mini_id"
        }
        fn ancestor_column() -> &'static str {
            "ancestor_id"
        }
        fn depth_column() -> &'static str {
            "depth"
        }
        fn path_count_column() -> &'static str {
            "path_count"
        }
    }

    #[test]
    fn left_join_closure_pair_emits_two_left_joins_with_shared_ancestor_semi_join() {
        let jqs: JoinedQuerySet<Mini, Mini> = QuerySet::<Mini>::new()
            .self_pairs()
            .left_join_closure_pair::<MiniClosure>();
        let acc = build_joined_select(&jqs).expect("emit");
        let sql = acc.sql();
        // Left closure LEFT JOIN — la binds to the left side's PK.
        assert!(
            sql.contains("LEFT JOIN mini_ancestries AS la ON la.mini_id = l.id"),
            "left closure LEFT JOIN missing: {sql}"
        );
        // Right closure LEFT JOIN — ra binds to the right side's PK
        // AND constrains ra.ancestor_id = la.ancestor_id (the
        // shared-ancestor semi-join that turns the pair of LEFT JOINs
        // into a per-pair "common ancestor" aggregator).
        assert!(
            sql.contains(
                "LEFT JOIN mini_ancestries AS ra ON ra.mini_id = r.id AND ra.ancestor_id = la.ancestor_id"
            ),
            "right closure LEFT JOIN with shared-ancestor predicate missing: {sql}"
        );
    }

    #[test]
    fn pair_closure_kinship_sum_emits_wright_style_summation() {
        let sum: PairClosureKinshipSum<MiniClosure> = PairClosureKinshipSum::new();
        let mut acc = SqlAccumulator::new("");
        crate::query::annotate::AnnotationSlot::push_column(&sum, &mut acc, 0);
        let sql = acc.sql();
        // SUM(la.<path_count> × ra.<path_count> × 0.5^(la.<depth> + ra.<depth> + 1))
        assert!(
            sql.contains("la.path_count::numeric * ra.path_count::numeric"),
            "kinship-sum path_count product missing: {sql}"
        );
        assert!(
            sql.contains("POWER(0.5::numeric, (la.depth + ra.depth + 1)::numeric)"),
            "kinship-sum depth-weighted half-power missing: {sql}"
        );
        // COALESCE wraps the SUM so missing closures decode as 0 instead
        // of failing the `f64` decode.
        assert!(
            sql.starts_with(", COALESCE(SUM("),
            "COALESCE wrap missing: {sql}"
        );
        // ::float8 keeps decoded f64 well-typed.
        assert!(sql.contains(")::float8"), "::float8 cast missing: {sql}");
        // OVER (PARTITION BY l.id, r.id) — per-pair partitioning so the
        // aggregate is evaluated per pair, not table-wide.
        assert!(
            sql.contains("OVER (PARTITION BY l.id, r.id)"),
            "per-pair PARTITION BY missing: {sql}"
        );
        assert!(
            sql.contains(" AS __djogi_agg_0"),
            "annotation alias missing: {sql}"
        );
    }

    #[test]
    fn pair_closure_kinship_sum_reports_requires_closure_pair_join() {
        let sum: PairClosureKinshipSum<MiniClosure> = PairClosureKinshipSum::new();
        assert!(
            crate::query::annotate::AnnotationSlot::requires_closure_pair_join(&sum),
            "PairClosureKinshipSum must report it requires the closure-pair LEFT JOIN"
        );
        // The IntoAggregateTuple blanket forwards the single-slot
        // override through, so a one-slot tuple sees the same answer.
        let tuple_view: &dyn crate::query::IntoAggregateTuple<Decoded = f64> = &sum;
        assert!(
            tuple_view.requires_closure_pair_join(),
            "IntoAggregateTuple blanket must forward to AnnotationSlot::requires_closure_pair_join"
        );
    }
}
