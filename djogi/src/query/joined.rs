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
//! projection pipeline never sees, which is exactly the gap GH #99 (substrate
//! for #84) tracks.
//!
//! The mating-pairs demo is the canonical validation target: until Step 3's
//! closure self-join + Wright F + window-fn ranking can be expressed without
//! `raw_rows`, the typed pair-tuple surface design is incomplete (per the
//! #99 acceptance criteria). This module plus its [`JoinedAnnotatedQuerySet`]
//! and closure-pair extensions contribute to that surface; the full
//! mating-pairs retrofit and Punnu showcase ride in follow-on tasks.
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
use crate::descriptor::PkType;
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
///
/// # Visibility
///
/// `#[doc(hidden)] pub` so the
/// [`crate::query::annotate::AnnotationSlot::validate_against_closure_pair`]
/// trait method (used by [`JoinedAnnotatedQuerySet::fetch_all`] to
/// detect closure-model mismatches across join + aggregate) can name
/// it without a private-in-public visibility leak. The struct is not
/// constructible outside the crate (every field is `pub(crate)` and
/// the only constructor is `left_join_closure_pair`), and
/// `AnnotationSlot` itself is sealed.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct ClosurePairJoin {
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
    // should agree on the column shape). They are read on the
    // mismatch-detection hot path by
    // `PairClosureKinshipSum<C>::validate_against_closure_pair` so a
    // `left_join_closure_pair::<C1>` paired with a
    // `PairClosureKinshipSum::<C2>` (C1 ≠ C2) gets rejected before
    // SQL build instead of surfacing as a Postgres
    // `42703 column does not exist`.
    /// Depth column (e.g. `"depth"`) — needed by typed aggregate emitters
    /// that walk the closure for Wright-style depth-weighted sums and by
    /// terminal-time identifier validation
    /// ([`Self::validate_idents`]).
    pub(crate) depth_column: &'static str,
    /// Path-multiplicity column (e.g. `"path_count"`). Used by
    /// [`PairClosureKinshipSum`]'s SUM emission and by terminal-time
    /// identifier validation ([`Self::validate_idents`]).
    pub(crate) path_count_column: &'static str,
}

impl ClosurePairJoin {
    /// Validate this closure-pair join's metadata identifiers against
    /// the Postgres unquoted-identifier contract AND the
    /// framework-reserved `__djogi_` prefix block before SQL emission.
    ///
    /// Each of `table`, `source_column`, `ancestor_column`,
    /// `depth_column`, `path_count_column` is checked via
    /// [`crate::ident::check_user_supplied_ident`] — the same gate
    /// [`crate::query::closure::validate_closure_metadata_idents`]
    /// applies to the `materialize_closure` SQL builder. Both code
    /// paths splice closure-model `&'static str` accessor return
    /// values into emitted SQL via `push_sql`; both need to validate
    /// before that splice or a hand-rolled `impl ClosureModel` with a
    /// hostile / malformed return value could smuggle SQL fragments
    /// through the accumulator.
    ///
    /// Called at terminal-call time (before
    /// [`build_joined_select`] / [`build_joined_count`] /
    /// [`build_joined_annotated_select_for_fetch`]) so a bad
    /// identifier surfaces as a typed [`DjogiError::Validation`]
    /// before any SQL string contains the offending bytes.
    pub(crate) fn validate_idents(&self) -> Result<(), DjogiError> {
        for (label, col) in [
            ("closure table", self.table),
            ("source_column", self.source_column),
            ("ancestor_column", self.ancestor_column),
            ("depth_column", self.depth_column),
            ("path_count_column", self.path_count_column),
        ] {
            crate::ident::check_user_supplied_ident(col, true).map_err(|e| {
                DjogiError::Validation(format!(
                    "ClosurePairJoin {label} identifier {col:?} rejected: {e:?}"
                ))
            })?;
        }
        Ok(())
    }
}

/// Resolve a model's primary-key column for pair-tuple paths that
/// reference per-row identity (`l.<pk>` / `r.<pk>` in WHERE, ON-clauses,
/// and GROUP BY emitters).
///
/// Returns `Ok(col)` only for **single-column** PK shapes:
/// [`PkType::HeerId`], [`PkType::RanjId`], [`PkType::HeerIdDesc`],
/// [`PkType::RanjIdDesc`], [`PkType::Serial`], and `Custom(_)` — all
/// inject a single `id` column on the source table.
///
/// Returns [`DjogiError::Validation`] for:
///
/// - [`PkType::None`] — the model has no primary key, so `l.<pk> <>
///   r.<pk>` self-pair anti-equality and `la.<src> = l.<pk>`
///   closure-pair joins both have no column to reference.
/// - [`PkType::Composite`] — multi-column PKs would need multi-column
///   `(l.c1, l.c2) <> (r.c1, r.c2)` emission, multi-column closure
///   join keys, and multi-column GROUP BY. The v0.1.0 pair-tuple
///   substrate does not implement multi-column emission, so composite
///   PKs are rejected at the terminal-call gate rather than silently
///   degenerating to a single-column emission (the pre-fix
///   `unwrap_or("id")` fallback).
///
/// Called from the terminals before SQL build:
/// [`JoinedQuerySet::fetch_all`], [`JoinedQuerySet::count`], and
/// [`JoinedAnnotatedQuerySet::fetch_all`] each invoke this for both
/// sides whenever the path requires per-row identity (self-pair
/// anti-equality, closure-pair joins, or pair-aggregate GROUP BY).
/// The validated `&'static str` is then threaded through the emitter
/// helpers as a parameter so no helper carries a silent
/// `unwrap_or("id")` fallback.
pub(crate) fn validated_pk_column_for_pair_identity<M: Model>() -> Result<&'static str, DjogiError>
{
    let desc = M::descriptor();
    match &desc.pk_type {
        PkType::HeerId
        | PkType::RanjId
        | PkType::HeerIdDesc
        | PkType::RanjIdDesc
        | PkType::Serial
        | PkType::Custom(_) => Ok(desc.pk_column().expect(
            "single-column PK shapes always produce a column via ModelDescriptor::pk_column",
        )),
        PkType::None => Err(DjogiError::Validation(format!(
            "model {} has no primary key (PkType::None); pair-tuple paths that reference \
             per-row identity (`l.<pk> <> r.<pk>` self-pair anti-equality, closure-pair \
             joins, or pair-aggregate GROUP BY) require a single-column PK. Either give \
             the model a PK or build the query through a path that does not need row \
             identity (cross-join with `include_equal_pk` and no closure-pair join).",
            M::table_name()
        ))),
        PkType::Composite(cols) => Err(DjogiError::Validation(format!(
            "model {} has a composite primary key ({:?}); the v0.1.0 pair-tuple substrate \
             does not emit multi-column anti-equality, closure-pair join keys, or GROUP BY. \
             A future slice will add multi-column emission; until then, pair-tuple paths \
             that require per-row identity are rejected at the terminal gate.",
            M::table_name(),
            cols
        ))),
    }
}

/// Validate per-row identity for both sides of a pair-tuple terminal
/// when the path needs it (self-pair anti-equality, closure-pair joins,
/// pair-aggregate GROUP BY).
///
/// Returns the validated `(left_pk, right_pk)` pair when validation
/// succeeds, or [`DjogiError::Validation`] when either side's PK shape
/// is unsupported. The terminal threads the resolved pair into
/// `build_joined_select` / `build_joined_count` /
/// `build_joined_annotated_select_for_fetch` as parameters so the
/// emitter helpers never re-look-up via `pk_column().unwrap_or("id")`.
///
/// `needs_identity` is the conjunction of the per-row-identity
/// triggers: `exclude_equal_pk` (self-pair WHERE), `closure_pair.is_some()`
/// (closure-pair LEFT JOIN ON-clauses), and any
/// `requires_closure_pair_join()` aggregate slot in the tuple (pair-
/// aggregate GROUP BY). The function short-circuits to `Ok((None, None))`
/// when none of these apply; the emitters then know per-row identity is
/// not needed and skip the per-side PK reference paths entirely.
pub(crate) fn validate_pair_identity_pk<L: Model, R: Model>(
    needs_identity: bool,
) -> Result<(Option<&'static str>, Option<&'static str>), DjogiError> {
    if !needs_identity {
        return Ok((None, None));
    }
    let l_pk = validated_pk_column_for_pair_identity::<L>()?;
    let r_pk = validated_pk_column_for_pair_identity::<R>()?;
    Ok((Some(l_pk), Some(r_pk)))
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

// ── Self-join-only methods (require L = R at compile time) ──────────
//
// Methods that only make structural sense on a single-model self-join
// (`(L, L)`) live in this impl block, which is reachable only when the
// two type parameters of `JoinedQuerySet<L, R>` are the same model.
// Heterogeneous pair queries (`(Animal, Widget)`) cannot reach these
// methods — the trait resolver does not consider this block for
// `JoinedQuerySet<Animal, Widget>` and the method is invisible at
// the call site.

/// Methods reachable only when the pair-tuple's two sides are the same
/// model — the typed entry surface for closure-self-join (Wright-style
/// shared-ancestor) queries.
///
/// `left_join_closure_pair` lives here (not on the L≠R impl block) so
/// the closure's `C: ClosureModel<Source = L>` bound is structurally
/// consistent with the queryset's right side: the right LEFT JOIN
/// emits `ra.<source_col> = r.<pk>` where `r` is also an `L` row,
/// which only makes sense when L = R. The previous v0.1.0 substrate
/// allowed this method on heterogeneous pair querysets and produced
/// bogus SQL that referenced `<L's closure>.<L's source> = <R's pk>`;
/// the type-level fix moves enforcement to compile time, so
/// `Animal::objects().cross_join_with(Widget::objects()).left_join_closure_pair::<AnimalAncestry>()`
/// fails to compile with `no method named left_join_closure_pair found
/// for struct JoinedQuerySet<Animal, Widget>`.
impl<L: Model> JoinedQuerySet<L, L> {
    /// Attach a closure-pair LEFT JOIN to this self-join for
    /// Wright-style shared-ancestor aggregation.
    ///
    /// The bound [`ClosureModel`] `C` names the closure table and
    /// column shape; the emitter splices:
    ///
    /// ```sql
    /// LEFT JOIN <closure_table> AS la ON la.<source_col> = l.<pk>
    /// LEFT JOIN <closure_table> AS ra ON ra.<source_col> = r.<pk>
    ///                              AND ra.<ancestor_col> = la.<ancestor_col>
    /// ```
    ///
    /// after the cross-join and before the WHERE clause. The right-side
    /// `AND ra.<ancestor> = la.<ancestor>` predicate is the
    /// shared-ancestor semi-join — only rows whose ancestor matches
    /// both sides survive, and `path_count` multiplicity stays per-side
    /// so the typed aggregate [`PairClosureKinshipSum::<C>`] can sum
    /// `la.path_count × ra.path_count × 0.5^(la.depth + ra.depth + 1)`
    /// across them.
    ///
    /// # Type-level L = R requirement
    ///
    /// This method only compiles when the pair-tuple's two sides are
    /// the same model. `C::Source = L` plus the impl-block's `L = R`
    /// witness means every alias substitution the closure-pair join
    /// emits is structurally well-defined: `la.<src> = l.<pk>` and
    /// `ra.<src> = r.<pk>` both bind to `L`'s primary key column, with
    /// `L`'s row identity on both sides.
    ///
    /// # Closure metadata validation
    ///
    /// The closure model's table name plus the four column accessors
    /// (`source_column`, `ancestor_column`, `depth_column`,
    /// `path_count_column`) are validated against the Postgres
    /// unquoted-identifier contract at terminal-call time
    /// ([`JoinedQuerySet::fetch_all`] / [`count`](Self::count) /
    /// [`JoinedAnnotatedQuerySet::fetch_all`]) via
    /// [`crate::query::closure::validate_closure_metadata_idents`] so a
    /// hand-rolled `impl ClosureModel` cannot smuggle SQL through the
    /// `push_sql` sites the closure-pair emitter calls. A bad identifier
    /// surfaces as [`DjogiError::Validation`] before SQL build.
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
            // Validate closure-pair metadata identifiers before SQL
            // build so a hand-rolled `impl ClosureModel` with bad
            // accessor strings surfaces as `DjogiError::Validation`
            // instead of letting the malformed SQL reach Postgres or
            // the bind binder. Same gate the `materialize_closure`
            // helper applies for its own SQL build.
            if let Some(cp) = self.closure_pair.as_ref() {
                cp.validate_idents()?;
            }
            // Validate per-row identity PK shape for both sides when
            // the path needs it (self-pair anti-equality or
            // closure-pair join). Rejects `PkType::None` and
            // `PkType::Composite` at the terminal gate so the emitters
            // can `expect` a single-column PK rather than silently
            // falling back to `"id"`. See
            // [`validated_pk_column_for_pair_identity`].
            let needs_identity = self.exclude_equal_pk || self.closure_pair.is_some();
            let (l_pk, r_pk) = validate_pair_identity_pk::<L, R>(needs_identity)?;
            // Auto-tenant: a pair-tuple query can be tenanted on the left
            // side, right side, or both. We dispatch sequentially with `?`
            // propagation so a misconfigured tenancy fails loudly before
            // the SQL round-trip.
            auto_set_tenant::<L>(ctx).await?;
            auto_set_tenant::<R>(ctx).await?;
            let acc = build_joined_select(&self, l_pk, r_pk).map_err(DjogiError::from)?;
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
            // Validate closure-pair metadata identifiers before SQL
            // build — same gate as fetch_all. The count emitter
            // currently omits the closure-pair LEFT JOINs (counting
            // semi-join cardinality is a future refinement), but the
            // identifiers were captured at builder time and are still
            // a public-API contract the caller can violate; reject
            // here for consistency with fetch_all's validation surface.
            if let Some(cp) = self.closure_pair.as_ref() {
                cp.validate_idents()?;
            }
            // Validate PK shape for the anti-equality path. The count
            // builder does NOT emit closure-pair LEFT JOINs, so the
            // closure-pair branch is not an identity trigger here —
            // only `exclude_equal_pk` is.
            let needs_identity = self.exclude_equal_pk;
            let (l_pk, r_pk) = validate_pair_identity_pk::<L, R>(needs_identity)?;
            auto_set_tenant::<L>(ctx).await?;
            auto_set_tenant::<R>(ctx).await?;
            let acc = build_joined_count(&self, l_pk, r_pk).map_err(DjogiError::from)?;
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

            // Validate closure-pair metadata identifiers before SQL
            // build — same gate as `JoinedQuerySet::fetch_all`. The
            // annotated path also splices `cp.table` / `cp.*_column`
            // into emitted SQL via `push_closure_pair_joins`, so a
            // hand-rolled `impl ClosureModel` with bad accessor
            // strings would smuggle SQL fragments without this check.
            if let Some(cp) = inner.closure_pair.as_ref() {
                cp.validate_idents()?;
            }

            // Validate the aggregate tuple's closure-pair metadata
            // against the queryset's closure-pair join. Catches:
            //   (1) hostile `impl ClosureModel` returning bad
            //       identifier strings from `PairClosureKinshipSum<C>`'s
            //       column accessors (closure metadata smuggling).
            //   (2) mismatch between `left_join_closure_pair::<C1>()`
            //       and `PairClosureKinshipSum::<C2>::new()` where C1
            //       ≠ C2 — the aggregate would reference closure
            //       column names the join clauses do not provide.
            // The default impl is a no-op for slots that don't carry
            // a `C: ClosureModel` parameter; only `PairClosureKinshipSum`
            // overrides it today.
            aggregates.validate_against_closure_pair(inner.closure_pair.as_ref())?;

            // Reject ordinary single-Model aggregates on joined
            // annotations until a pair-aware API exists. Slot impls
            // opt into joined-context safety by overriding
            // `AnnotationSlot::is_joined_safe`; tuple impls AND
            // across slots. `AggregateExpr<V>` keeps the default
            // `false` because its emitted SQL is a bare-column
            // expression (`SUM(age) OVER ()`) that's ambiguous in
            // joined contexts where both sides may share columns.
            // See the doc comment on `is_joined_safe`.
            if !aggregates.is_joined_safe() {
                return Err(DjogiError::Validation(
                    "joined-queryset `.annotate(...)` rejected an annotation slot that is \
                     not joined-context-safe. Ordinary single-Model aggregates \
                     (e.g. `l.age().sum()`) emit a bare column reference like `SUM(age) \
                     OVER ()` that is ambiguous when both pair sides share column names \
                     (always true for self-joins). Use a pair-aware annotation: \
                     `PairClosureKinshipSum<C>` for kinship summation, or a window \
                     function with `partition_by_pair(PairSide::Left, ...)` / \
                     `order_by_pair_asc(PairSide::Right, ...)`. A pair-aware ordinary \
                     aggregate surface is a future slice."
                        .to_string(),
                ));
            }

            // Validate PK shape for any identity-required path:
            // `exclude_equal_pk` (self-pair anti-equality),
            // `closure_pair.is_some()` (closure-pair LEFT JOINs),
            // or `requires_closure_pair_join()` (pair-aggregate
            // GROUP BY). Rejects `PkType::None` / `PkType::Composite`
            // at the gate so the emitters never silently fall back.
            let needs_identity = inner.exclude_equal_pk
                || inner.closure_pair.is_some()
                || aggregates.requires_closure_pair_join();
            let (l_pk, r_pk) = validate_pair_identity_pk::<L, R>(needs_identity)?;

            auto_set_tenant::<L>(ctx).await?;
            auto_set_tenant::<R>(ctx).await?;

            let acc = build_joined_annotated_select_for_fetch(
                &inner,
                &aggregates,
                qualify.as_ref(),
                l_pk,
                r_pk,
            )
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
    l_pk: Option<&'static str>,
    r_pk: Option<&'static str>,
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

    push_closure_pair_joins::<L, R>(&mut acc, jqs, l_pk, r_pk);
    push_joined_where::<L, R>(&mut acc, jqs, l_pk, r_pk)?;
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
    l_pk: Option<&'static str>,
    r_pk: Option<&'static str>,
) -> Result<SqlAccumulator, crate::query::PortablePredicateError> {
    let mut acc = SqlAccumulator::new("SELECT COUNT(*) FROM ");
    acc.push_sql(L::table_name());
    acc.push_sql(" AS ");
    acc.push_sql(LEFT_ALIAS);
    acc.push_sql(" CROSS JOIN ");
    acc.push_sql(R::table_name());
    acc.push_sql(" AS ");
    acc.push_sql(RIGHT_ALIAS);
    push_joined_where::<L, R>(&mut acc, jqs, l_pk, r_pk)?;
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
    l_pk: Option<&'static str>,
    r_pk: Option<&'static str>,
) -> Result<SqlAccumulator, crate::query::PortablePredicateError>
where
    L: Model + crate::pg::decode::FromPgRow,
    R: Model + crate::pg::decode::FromPgRow,
    A: IntoAggregateTuple,
{
    let inner = build_joined_annotated_inner::<L, R, A>(jqs, aggregates, l_pk, r_pk)?;
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
    l_pk: Option<&'static str>,
    r_pk: Option<&'static str>,
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

    push_closure_pair_joins::<L, R>(&mut acc, jqs, l_pk, r_pk);
    push_joined_where::<L, R>(&mut acc, jqs, l_pk, r_pk)?;

    // When the queryset has a closure-pair LEFT JOIN AND the
    // annotation tuple includes a closure-pair-requiring aggregate
    // (today: `PairClosureKinshipSum`), aggregate per pair via
    // `GROUP BY l.<pk>, r.<pk>` so the closure-row multiplicity (one
    // SQL row per shared ancestor) collapses to one output row per
    // `(L, R)` pair. The aggregate's `push_column` emits the bare
    // SUM body (no OVER) so the outer GROUP BY drives aggregation
    // cleanly. Postgres 18 honours functional-dependency rules:
    // every non-aggregate column on the SELECT list is functionally
    // determined by the GROUP BY (`l.id` is L's PK, `r.id` is R's
    // PK, so `l.*` and `r.*` are determined and need not be named
    // in GROUP BY explicitly).
    push_joined_group_by_if_needed::<L, R, A>(&mut acc, jqs, aggregates, l_pk, r_pk);

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

/// Emit `GROUP BY <l_alias>.<l_pk>, <r_alias>.<r_pk>` when the
/// annotated joined queryset needs per-pair aggregation.
///
/// Today the only annotation type that triggers this is
/// [`PairClosureKinshipSum`]; the generic check `requires_closure_pair_join`
/// captures the same predicate. The PK columns are passed in pre-
/// validated by [`validated_pk_column_for_pair_identity`] — the terminal
/// runs that gate at terminal-call time, so a `None` in either side's
/// slot here is an emitter-internal invariant break (the terminal
/// should have rejected the path before SQL build).
///
/// Note on simple-PK assumption: the `l.<pk>, r.<pk>` partition shape
/// only handles single-column PKs (HeerId / RanjId / Serial /
/// HeerIdDesc / RanjIdDesc / Custom). Composite-PK models would need
/// multi-column GROUP BY emission; that case is rejected by
/// [`validated_pk_column_for_pair_identity`] before reaching this
/// helper, so a `Composite` PK would surface as a typed
/// [`DjogiError::Validation`] at the terminal rather than silently
/// degenerating to first-column-only GROUP BY (the pre-fix
/// `unwrap_or("id")` behaviour).
fn push_joined_group_by_if_needed<L, R, A>(
    acc: &mut SqlAccumulator,
    jqs: &JoinedQuerySet<L, R>,
    aggregates: &A,
    l_pk: Option<&'static str>,
    r_pk: Option<&'static str>,
) where
    L: Model,
    R: Model,
    A: IntoAggregateTuple,
{
    if !aggregates.requires_closure_pair_join() || jqs.closure_pair.is_none() {
        return;
    }
    let l_pk = l_pk.expect(
        "terminal must validate left PK via validated_pk_column_for_pair_identity \
         when the aggregate tuple requires a closure-pair join",
    );
    let r_pk = r_pk.expect(
        "terminal must validate right PK via validated_pk_column_for_pair_identity \
         when the aggregate tuple requires a closure-pair join",
    );
    acc.push_sql(" GROUP BY ");
    acc.push_sql(LEFT_ALIAS);
    acc.push_sql(".");
    acc.push_sql(l_pk);
    acc.push_sql(", ");
    acc.push_sql(RIGHT_ALIAS);
    acc.push_sql(".");
    acc.push_sql(r_pk);
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

/// Emit the two `LEFT JOIN <closure_table>` clauses for a closure-pair
/// queryset (`la` and `ra` aliases) when the queryset has a
/// `ClosurePairJoin` set. No-op otherwise.
///
/// PK columns are pre-validated by
/// [`validated_pk_column_for_pair_identity`] — the terminal runs that
/// gate at terminal-call time so a None here is an emitter-internal
/// invariant break. The `expect` calls point at the validator.
fn push_closure_pair_joins<L: Model, R: Model>(
    acc: &mut SqlAccumulator,
    jqs: &JoinedQuerySet<L, R>,
    l_pk: Option<&'static str>,
    r_pk: Option<&'static str>,
) {
    let Some(cp) = jqs.closure_pair.as_ref() else {
        return;
    };
    let l_pk = l_pk.expect(
        "terminal must validate left PK via validated_pk_column_for_pair_identity \
         before emitting closure-pair LEFT JOIN ON-clauses",
    );
    let r_pk = r_pk.expect(
        "terminal must validate right PK via validated_pk_column_for_pair_identity \
         before emitting closure-pair LEFT JOIN ON-clauses",
    );

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

/// Emit the joined-queryset `WHERE` clause — anti-equality (when
/// `exclude_equal_pk` is set) plus the left and right side conditions.
///
/// `l_pk` / `r_pk` are passed in pre-validated by
/// [`validated_pk_column_for_pair_identity`] when the path needs the
/// `l.<pk> <> r.<pk>` anti-equality (i.e. `exclude_equal_pk` is true).
/// When the anti-equality is not in scope, both PK columns are
/// permitted to be `None` because no identity column is emitted.
fn push_joined_where<L: Model, R: Model>(
    acc: &mut SqlAccumulator,
    jqs: &JoinedQuerySet<L, R>,
    l_pk: Option<&'static str>,
    r_pk: Option<&'static str>,
) -> Result<(), crate::query::PortablePredicateError> {
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
                let l_pk = l_pk.expect(
                    "terminal must validate left PK via validated_pk_column_for_pair_identity \
                     before emitting self-pair anti-equality (exclude_equal_pk=true)",
                );
                let r_pk = r_pk.expect(
                    "terminal must validate right PK via validated_pk_column_for_pair_identity \
                     before emitting self-pair anti-equality (exclude_equal_pk=true)",
                );
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
        // Bare aggregate emission — no `OVER (...)` window clause.
        //
        // Earlier substrate emitted `SUM(...) OVER (PARTITION BY
        // l.<pk>, r.<pk>)` so the kinship value was the same on every
        // row of a pair's `(la, ra)` cross-product. That kept the
        // aggregate value correct per row but DID NOT collapse the
        // cross-product to one row per pair — the SELECT decode loop
        // happily pushed every `(L, R, la_row, ra_row)` quadruple into
        // the output `Vec<((L, R), f64)>`, returning M×N duplicates
        // per pair when both sides had multiple closure rows for a
        // shared ancestor.
        //
        // The fix uses a true `GROUP BY l.<pk>, r.<pk>` (emitted by
        // `build_joined_annotated_inner`'s `requires_closure_pair_join`
        // path) so the aggregate collapses to one row per pair, and
        // every non-aggregate SELECT-list column is functionally
        // determined by the GROUP BY (Postgres 18 honours the
        // PK-functional-dependency rule for `l.id` / `r.id` against
        // `l.*` / `r.*`). The aggregate emitted here is the bare SUM
        // body — no OVER clause — so it composes cleanly with the
        // outer GROUP BY.
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

    /// `PairClosureKinshipSum`'s emitted SQL references closure-pair
    /// aliases (`la.<col>` / `ra.<col>`) which are pair-side-specific by
    /// construction — there is no bare-column ambiguity even in
    /// self-join contexts.
    fn is_joined_safe(&self) -> bool {
        true
    }

    /// Validate the closure model `C` named on this aggregate against
    /// the queryset's closure-pair join.
    ///
    /// Runs the same identifier gate `materialize_closure` uses on its
    /// own column accessors (via
    /// [`crate::query::closure::validate_closure_metadata_idents`]) so a
    /// hand-rolled hostile `impl ClosureModel` cannot smuggle SQL
    /// fragments through the aggregate's `push_sql` sites.
    ///
    /// When `closure_pair` is `Some(cp)`, also enforces that the
    /// aggregate's `C` matches the join's `C` by comparing every
    /// captured identifier on `cp` against `C`'s same-named accessor.
    /// This catches the
    /// `left_join_closure_pair::<C1>() ... PairClosureKinshipSum::<C2>::new()`
    /// (C1 ≠ C2) mismatch case before SQL build — the aggregate
    /// would otherwise reference closure columns the join clauses do
    /// not provide and surface as a Postgres `42703 column does not
    /// exist` error at execute time.
    fn validate_against_closure_pair(
        &self,
        closure_pair: Option<&crate::query::joined::ClosurePairJoin>,
    ) -> Result<(), crate::DjogiError> {
        // (1) C's own identifier validation — always run, even when
        // the queryset has no closure-pair join. The terminal's
        // `requires_closure_pair_join` gate would already reject the
        // missing-join case before reaching SQL emission, but a hostile
        // `C` could still corrupt diagnostic strings; better to gate
        // before any `push_sql` of `C::*_column()` returns.
        crate::query::closure::validate_closure_metadata_idents::<C>()?;

        // (2) Aggregate-C vs join-C metadata mismatch — only meaningful
        // when a closure-pair join is captured.
        if let Some(cp) = closure_pair {
            let c_table = C::table();
            let c_source = C::source_column();
            let c_ancestor = C::ancestor_column();
            let c_depth = C::depth_column();
            let c_path_count = C::path_count_column();
            if cp.table != c_table
                || cp.source_column != c_source
                || cp.ancestor_column != c_ancestor
                || cp.depth_column != c_depth
                || cp.path_count_column != c_path_count
            {
                return Err(crate::DjogiError::Validation(format!(
                    "ClosureModel mismatch on PairClosureKinshipSum: \
                     `left_join_closure_pair::<...>()` captured \
                     {{ table: {join_table:?}, source: {join_source:?}, \
                     ancestor: {join_ancestor:?}, depth: {join_depth:?}, \
                     path_count: {join_path_count:?} }} but \
                     PairClosureKinshipSum<{c_type}> emits \
                     {{ table: {c_table:?}, source: {c_source:?}, \
                     ancestor: {c_ancestor:?}, depth: {c_depth:?}, \
                     path_count: {c_path_count:?} }}. \
                     The aggregate and the join must reference the same \
                     ClosureModel — use the same `C` type parameter on both \
                     `left_join_closure_pair::<C>()` and `PairClosureKinshipSum::<C>::new()`.",
                    join_table = cp.table,
                    join_source = cp.source_column,
                    join_ancestor = cp.ancestor_column,
                    join_depth = cp.depth_column,
                    join_path_count = cp.path_count_column,
                    c_type = std::any::type_name::<C>(),
                    c_table = c_table,
                    c_source = c_source,
                    c_ancestor = c_ancestor,
                    c_depth = c_depth,
                    c_path_count = c_path_count,
                )));
            }
        }
        Ok(())
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

    /// Test helper: pre-validate PK shape (as the terminals do at
    /// runtime) and emit the joined SELECT. Mirrors the
    /// `JoinedQuerySet::fetch_all` glue so test bodies stay focused on
    /// SQL shape rather than on threading the validator + emitter.
    fn build_joined_select_for_test<L, R>(
        jqs: &JoinedQuerySet<L, R>,
    ) -> Result<SqlAccumulator, crate::query::PortablePredicateError>
    where
        L: Model + crate::pg::decode::FromPgRow,
        R: Model + crate::pg::decode::FromPgRow,
    {
        let needs_identity = jqs.exclude_equal_pk || jqs.closure_pair.is_some();
        let (l_pk, r_pk) = validate_pair_identity_pk::<L, R>(needs_identity)
            .expect("test fixtures use single-column PKs that pass the validator");
        build_joined_select(jqs, l_pk, r_pk)
    }

    /// Test helper companion for [`build_joined_count`] — same
    /// validator gate as the count terminal.
    fn build_joined_count_for_test<L, R>(
        jqs: &JoinedQuerySet<L, R>,
    ) -> Result<SqlAccumulator, crate::query::PortablePredicateError>
    where
        L: Model,
        R: Model,
    {
        let needs_identity = jqs.exclude_equal_pk;
        let (l_pk, r_pk) = validate_pair_identity_pk::<L, R>(needs_identity)
            .expect("test fixtures use single-column PKs that pass the validator");
        build_joined_count(jqs, l_pk, r_pk)
    }

    /// Test helper companion for
    /// [`build_joined_annotated_select_for_fetch`] — runs the same
    /// validator gate the annotated terminal does.
    fn build_joined_annotated_select_for_fetch_for_test<L, R, A>(
        jqs: &JoinedQuerySet<L, R>,
        aggregates: &A,
        qualify: Option<&crate::expr::QualifyCondition>,
    ) -> Result<SqlAccumulator, crate::query::PortablePredicateError>
    where
        L: Model + crate::pg::decode::FromPgRow,
        R: Model + crate::pg::decode::FromPgRow,
        A: IntoAggregateTuple,
    {
        let needs_identity = jqs.exclude_equal_pk
            || jqs.closure_pair.is_some()
            || aggregates.requires_closure_pair_join();
        let (l_pk, r_pk) = validate_pair_identity_pk::<L, R>(needs_identity)
            .expect("test fixtures use single-column PKs that pass the validator");
        build_joined_annotated_select_for_fetch(jqs, aggregates, qualify, l_pk, r_pk)
    }

    #[test]
    fn self_pairs_emits_cross_join_and_exclude_equal_pk() {
        let jqs: JoinedQuerySet<Mini, Mini> = QuerySet::<Mini>::new().self_pairs();
        let acc = build_joined_select_for_test(&jqs).expect("emit");
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
        let acc = build_joined_select_for_test(&jqs).expect("emit");
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
        let acc = build_joined_select_for_test(&jqs).expect("emit");
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
        let acc = build_joined_select_for_test(&jqs).expect("emit");
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
        let acc = build_joined_select_for_test(&jqs).expect("emit");
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
        let acc = build_joined_count_for_test(&jqs).expect("emit");
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
        let acc = build_joined_select_for_test(&jqs).expect("emit");
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
        // No OVER clause — per-pair aggregation is now driven by the
        // outer GROUP BY (`build_joined_annotated_inner` emits
        // `GROUP BY l.<pk>, r.<pk>` when the aggregate tuple reports
        // `requires_closure_pair_join`). Window form was emitting
        // M×N rows per pair when both sides had multiple closure rows
        // sharing an ancestor; the bare aggregate + GROUP BY collapses
        // the closure cross-product to one output row per pair.
        assert!(
            !sql.contains("OVER ("),
            "kinship-sum must NOT emit window OVER clause — outer GROUP BY drives aggregation: {sql}"
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

    // ── GPT-5.5 xhigh round-1 fixes — coverage backfill ─────────────────
    //
    // The xhigh review surfaced five merge blockers in the substrate
    // commit. Each fix below adds a focused unit test that pins the
    // intended behaviour after the fix so the next regression surfaces
    // in `cargo test -p djogi --lib query::joined::tests` instead of
    // taking the production code with it.

    #[test]
    fn build_joined_annotated_inner_emits_group_by_when_kinship_aggregate_requires_pair_join() {
        // Kinship aggregation now uses bare SUM + outer GROUP BY
        // l.<pk>, r.<pk>. Previously the aggregate emitted `OVER
        // (PARTITION BY l.id, r.id)` so the SQL value was correct
        // per row but the row count fell out as `|L| × |R| ×
        // |la_rows| × |ra_rows|` — every pair surfaced once per
        // matching closure cross-product entry. The GROUP BY collapses
        // back to one row per pair.
        let jqs: JoinedQuerySet<Mini, Mini> = QuerySet::<Mini>::new()
            .self_pairs()
            .left_join_closure_pair::<MiniClosure>();
        let annotated = jqs.annotate(|_l, _r| PairClosureKinshipSum::<MiniClosure>::new());
        let acc = build_joined_annotated_select_for_fetch_for_test(
            &annotated.inner,
            &annotated.aggregates,
            annotated.qualify.as_ref(),
        )
        .expect("emit");
        let sql = acc.sql();
        assert!(
            sql.contains(" GROUP BY l.id, r.id"),
            "per-pair GROUP BY must be emitted when closure-pair aggregate is in the tuple: {sql}"
        );
        // And bare aggregate (no OVER) — the GROUP BY drives
        // aggregation, not a window partition.
        assert!(
            !sql.contains("OVER ("),
            "kinship-sum must emit bare aggregate when GROUP BY is in scope: {sql}"
        );
    }

    #[test]
    fn build_joined_annotated_inner_omits_group_by_when_no_pair_aggregate() {
        // Plain self-join + window annotate (no closure-pair aggregate)
        // does NOT emit GROUP BY. The GROUP BY emission is gated on
        // `requires_closure_pair_join()`, so ordinary annotated joined
        // queries keep their per-row SELECT shape.
        let jqs: JoinedQuerySet<Mini, Mini> = QuerySet::<Mini>::new().self_pairs();
        let annotated = jqs.annotate(|_l, _r| {
            crate::expr::RowNumber::new()
                .partition_by_pair(
                    PairSide::Left,
                    crate::query::FieldRef::<Mini, HeerId>::new("id"),
                )
                .alias("rank")
        });
        let acc = build_joined_annotated_select_for_fetch_for_test(
            &annotated.inner,
            &annotated.aggregates,
            annotated.qualify.as_ref(),
        )
        .expect("emit");
        let sql = acc.sql();
        assert!(
            !sql.contains("GROUP BY"),
            "non-kinship annotated joined query must NOT emit GROUP BY: {sql}"
        );
    }

    #[test]
    fn closure_pair_join_validate_idents_accepts_clean_metadata() {
        // MiniClosure's accessors are all plain identifiers — should
        // validate cleanly.
        let cp = ClosurePairJoin {
            table: "mini_ancestries",
            source_column: "mini_id",
            ancestor_column: "ancestor_id",
            depth_column: "depth",
            path_count_column: "path_count",
        };
        assert!(cp.validate_idents().is_ok());
    }

    #[test]
    fn closure_pair_join_validate_idents_rejects_sql_injection_table() {
        // A hostile / typo'd ClosureModel that returned a "table name"
        // containing whitespace would smuggle SQL into the LEFT JOIN
        // emission. `validate_idents` catches this before the SQL
        // string ever reaches the accumulator.
        let cp = ClosurePairJoin {
            table: "ancestries; DROP TABLE users;--",
            source_column: "mini_id",
            ancestor_column: "ancestor_id",
            depth_column: "depth",
            path_count_column: "path_count",
        };
        let err = cp.validate_idents().expect_err("must reject");
        match err {
            DjogiError::Validation(msg) => {
                assert!(
                    msg.contains("closure table") && msg.contains("DROP TABLE"),
                    "validation error must name the offending field + content: {msg}"
                );
            }
            other => panic!("expected DjogiError::Validation, got {other:?}"),
        }
    }

    #[test]
    fn closure_pair_join_validate_idents_rejects_reserved_djogi_prefix() {
        // A user-supplied identifier that lands in the framework-
        // reserved `__djogi_` namespace is rejected — same gate the
        // materialize-closure path uses.
        let cp = ClosurePairJoin {
            table: "mini_ancestries",
            source_column: "__djogi_internal_source",
            ancestor_column: "ancestor_id",
            depth_column: "depth",
            path_count_column: "path_count",
        };
        let err = cp.validate_idents().expect_err("must reject");
        match err {
            DjogiError::Validation(msg) => {
                assert!(
                    msg.contains("source_column") && msg.contains("__djogi_internal_source"),
                    "validation error must name the offending column: {msg}"
                );
            }
            other => panic!("expected DjogiError::Validation, got {other:?}"),
        }
    }

    #[test]
    fn closure_pair_join_validate_idents_rejects_each_column_in_turn() {
        // Cover every column slot. Each malformed identifier should
        // surface labelled with its slot name so an on-call engineer
        // can map the validation error back to the offending
        // ClosureModel impl method.
        for (which, bad_label, mut cp) in [
            (
                "ancestor_column",
                "ancestor_column",
                ClosurePairJoin {
                    table: "mini_ancestries",
                    source_column: "mini_id",
                    ancestor_column: "0bad",
                    depth_column: "depth",
                    path_count_column: "path_count",
                },
            ),
            (
                "depth_column",
                "depth_column",
                ClosurePairJoin {
                    table: "mini_ancestries",
                    source_column: "mini_id",
                    ancestor_column: "ancestor_id",
                    depth_column: "0bad",
                    path_count_column: "path_count",
                },
            ),
            (
                "path_count_column",
                "path_count_column",
                ClosurePairJoin {
                    table: "mini_ancestries",
                    source_column: "mini_id",
                    ancestor_column: "ancestor_id",
                    depth_column: "depth",
                    path_count_column: "0bad",
                },
            ),
        ] {
            // suppress unused-var warning by reading once
            let _ = (&which, &mut cp);
            let err = cp.validate_idents().expect_err("must reject");
            let msg = match err {
                DjogiError::Validation(s) => s,
                other => panic!("expected DjogiError::Validation, got {other:?}"),
            };
            assert!(
                msg.contains(bad_label),
                "{which}: must label offending slot, got {msg}"
            );
        }
    }

    #[test]
    fn left_join_closure_pair_fetch_all_rejects_bad_closure_metadata() {
        use crate::context::DjogiContext;
        // Hand-roll a hostile ClosureModel whose path_count_column
        // returns an injection string. The validation gate runs before
        // SQL build, so the fetch_all terminal returns
        // DjogiError::Validation rather than reaching the bind binder.
        struct HostileClosure;
        impl crate::model::__sealed::Sealed for HostileClosure {}
        #[allow(clippy::manual_async_fn)]
        impl crate::model::Model for HostileClosure {
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
        impl FromPgRow for HostileClosure {
            const COLUMNS: &'static [&'static str] =
                &["mini_id", "ancestor_id", "depth", "path_count"];
            const COLUMN_LIST: &'static str = "mini_id, ancestor_id, depth, path_count";
            fn from_pg_row(_row: &tokio_postgres::Row) -> Result<Self, DjogiError> {
                unreachable!()
            }
        }
        impl crate::query::ClosureModel for HostileClosure {
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
                // Smuggled SQL fragment. `validate_idents` should
                // reject this — same gate the `materialize_closure`
                // helper applies for its own identifier accessors.
                "path_count; DROP TABLE users;--"
            }
        }
        let jqs: JoinedQuerySet<Mini, Mini> = QuerySet::<Mini>::new()
            .self_pairs()
            .left_join_closure_pair::<HostileClosure>();
        // Validate directly on the captured ClosurePairJoin — this is
        // the exact call the terminal makes before SQL build.
        let cp = jqs.closure_pair.as_ref().expect("closure_pair set");
        let err = cp.validate_idents().expect_err("must reject");
        match err {
            DjogiError::Validation(msg) => {
                assert!(
                    msg.contains("path_count_column"),
                    "must label offending slot: {msg}"
                );
            }
            other => panic!("expected DjogiError::Validation, got {other:?}"),
        }
    }

    // ── Blocker 1 — Heterogeneous closure-pair join compile-fail ───────
    //
    // The fix moves `left_join_closure_pair` from the `impl<L, R>` block
    // to `impl<L> JoinedQuerySet<L, L>` so a heterogeneous pair
    // (`cross_join_with(other_model)`) cannot reach the method. The
    // companion compile-fail fixture in
    // `djogi-macros/tests/compile_fail/phase8_5_cluster4a_heterogeneous_closure_pair_rejected.rs`
    // exercises this at lihaaf-fixture level — the compiler emits
    // `no method named left_join_closure_pair found for struct
    // JoinedQuerySet<Animal, Widget>`.
    //
    // Below is a compile-pass test that pins the L = L self-join entry
    // shape stays reachable. If a future refactor accidentally narrows
    // the L = L impl block (e.g. requires additional bounds the
    // self-join entry doesn't carry), this test stops compiling.

    #[test]
    fn left_join_closure_pair_compiles_on_self_pairs() {
        let _jqs: JoinedQuerySet<Mini, Mini> = QuerySet::<Mini>::new()
            .self_pairs()
            .left_join_closure_pair::<MiniClosure>();
    }

    // ── Blocker 4 — expression filter alias qualification ──────────────

    #[test]
    fn filter_expr_on_joined_query_emits_alias_qualified_column() {
        use crate::expr::Expr;
        // Direct test of the joined `Q::Expression(_)` emission path.
        // Build a Q::Expression wrapping an Expr<bool> whose LHS is a
        // bare field reference, then drop it through `emit_q::<Mini>`
        // with `SqlEmitContext::joined("l")`. The pre-fix emitter
        // pushed the bare column string, so the joined WHERE rendered
        // `name = $1` — Postgres flags it as ambiguous when L and R
        // share columns (a self-join always does). The fix threads
        // `SqlEmitContext` through `Q::Expression(_)` so bare column
        // refs in expression filters qualify with the side's alias.
        let name_ref: crate::query::FieldRef<Mini, String> = crate::query::FieldRef::new("name");
        let bool_expr: Expr<bool> = name_ref.as_expr().eq(Expr::literal("alpha".to_string()));
        // Wrap as a `Q::Expression(_)` — the exact arm the joined
        // WHERE emitter walks when `filter_expr` (or `Q::Expression`)
        // contributes a clause.
        let q: crate::query::Q<Mini> = crate::query::Q::Expression(bool_expr);

        // Joined-context emission ("l" alias) must qualify the column.
        let mut acc_left = crate::pg::accumulator::SqlAccumulator::new("");
        crate::query::sql::emit_q::<Mini>(
            &mut acc_left,
            &q,
            crate::query::SqlEmitContext::joined(LEFT_ALIAS),
        )
        .expect("emit");
        let sql_left = acc_left.sql();
        assert!(
            sql_left.contains("l.name = $"),
            "joined-context filter_expr must alias-qualify as `l.name`: {sql_left}"
        );

        // Same expression under the right side's alias.
        let mut acc_right = crate::pg::accumulator::SqlAccumulator::new("");
        crate::query::sql::emit_q::<Mini>(
            &mut acc_right,
            &q,
            crate::query::SqlEmitContext::joined(RIGHT_ALIAS),
        )
        .expect("emit");
        let sql_right = acc_right.sql();
        assert!(
            sql_right.contains("r.name = $"),
            "joined-context filter_expr must alias-qualify as `r.name`: {sql_right}"
        );

        // Sanity: root context still emits bare (non-joined queries).
        let mut acc_root = crate::pg::accumulator::SqlAccumulator::new("");
        crate::query::sql::emit_q::<Mini>(&mut acc_root, &q, crate::query::SqlEmitContext::root())
            .expect("emit");
        let sql_root = acc_root.sql();
        assert!(
            sql_root.starts_with("name = $"),
            "root-context emission must remain bare (Phase 2 contract): {sql_root}"
        );
    }

    // ── Blocker 5 — pair aggregate on single-Model annotate ────────────
    //
    // The single-Model `AnnotatedQuerySet::fetch_all` is async-only
    // (needs a `DjogiContext`), so this case is exercised in
    // integration tests when a live DB is available. The unit
    // surface below verifies the `requires_closure_pair_join`
    // signal is propagated through `IntoAggregateTuple` correctly —
    // single-Model terminals consult the same flag.

    #[test]
    fn pair_aggregate_into_aggregate_tuple_signals_pair_only() {
        let sum: PairClosureKinshipSum<MiniClosure> = PairClosureKinshipSum::new();
        let tuple_view: &dyn crate::query::IntoAggregateTuple<Decoded = f64> = &sum;
        assert!(
            tuple_view.requires_closure_pair_join(),
            "PairClosureKinshipSum reports it requires a closure-pair join through the tuple bridge \
             — single-Model annotate's `requires_closure_pair_join` check uses this same signal \
             to reject the slot at terminal-call time"
        );
    }

    // ── GPT-5.5 xhigh round-2 fixes — coverage backfill ──────────────────
    //
    // Round 2 surfaced four merge blockers in the round-1 substrate:
    //   1. PairClosureKinshipSum<C> emitted C's metadata without
    //      validation, and no gate tied the aggregate's C to the join's C.
    //   2. The emitters fell back to `pk_column().unwrap_or("id")` for
    //      `PkType::None` / `PkType::Composite` models — silent
    //      degradation rather than a typed error.
    //   3. Joined annotations accepted ordinary `AggregateExpr<V>` which
    //      emits bare-column SQL — ambiguous on self-joins.
    //   4. Source comments and fixture comments used GH close keywords
    //      against #99 / #84 (fixed via wording sweep — covered by grep
    //      verification at commit time rather than a unit test).

    // ── Blocker 1.A — PairClosureKinshipSum<C> validates C's metadata ────

    #[test]
    fn pair_closure_kinship_sum_validates_clean_closure_metadata() {
        // Sanity: a clean ClosureModel passes the aggregate's validator
        // both standalone (closure_pair = None) and when paired with a
        // matching captured closure.
        let sum: PairClosureKinshipSum<MiniClosure> = PairClosureKinshipSum::new();
        assert!(
            crate::query::annotate::AnnotationSlot::validate_against_closure_pair(&sum, None)
                .is_ok(),
            "clean ClosureModel passes the standalone metadata gate"
        );

        let matching_cp = ClosurePairJoin {
            table: MiniClosure::table(),
            source_column: MiniClosure::source_column(),
            ancestor_column: MiniClosure::ancestor_column(),
            depth_column: MiniClosure::depth_column(),
            path_count_column: MiniClosure::path_count_column(),
        };
        assert!(
            crate::query::annotate::AnnotationSlot::validate_against_closure_pair(
                &sum,
                Some(&matching_cp)
            )
            .is_ok(),
            "matching ClosureModel passes the join-C-vs-aggregate-C gate"
        );
    }

    #[test]
    fn pair_closure_kinship_sum_rejects_hostile_closure_metadata_in_aggregate() {
        // Hand-roll a hostile ClosureModel whose `path_count_column`
        // returns a SQL-injection string. The aggregate's emitter would
        // splice that string directly via `push_sql`, so the validator
        // must reject before any SQL is emitted — same gate
        // `materialize_closure` uses for its own column accessors.
        struct HostileForAggregate;
        impl crate::model::__sealed::Sealed for HostileForAggregate {}
        #[allow(clippy::manual_async_fn)]
        impl crate::model::Model for HostileForAggregate {
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
        impl FromPgRow for HostileForAggregate {
            const COLUMNS: &'static [&'static str] =
                &["mini_id", "ancestor_id", "depth", "path_count"];
            const COLUMN_LIST: &'static str = "mini_id, ancestor_id, depth, path_count";
            fn from_pg_row(_row: &tokio_postgres::Row) -> Result<Self, DjogiError> {
                unreachable!()
            }
        }
        impl crate::query::ClosureModel for HostileForAggregate {
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
                "path_count) FROM pg_class; --"
            }
        }
        let sum: PairClosureKinshipSum<HostileForAggregate> = PairClosureKinshipSum::new();
        let err = crate::query::annotate::AnnotationSlot::validate_against_closure_pair(&sum, None)
            .expect_err("hostile aggregate C must be rejected before SQL emission");
        match err {
            DjogiError::Validation(msg) => {
                assert!(
                    msg.contains("path_count_column") || msg.contains("path_count)"),
                    "validation error must point at the offending C accessor: {msg}"
                );
            }
            other => panic!("expected DjogiError::Validation, got {other:?}"),
        }
    }

    // ── Blocker 1.B — Mismatched aggregate-C vs join-C is rejected ───────

    #[test]
    fn pair_closure_kinship_sum_rejects_mismatched_closure_join() {
        // Synthesise a closure-pair join captured for one ClosureModel,
        // paired with a PairClosureKinshipSum aggregate for a *different*
        // ClosureModel. The aggregate's emitted SQL would reference
        // closure column names the join clauses do not provide — Postgres
        // would surface a `42703 column does not exist` at execute time.
        // The validate_against_closure_pair gate must catch this
        // before SQL build.
        //
        // We construct the join descriptor by hand with different column
        // names than `MiniClosure` exposes; `PairClosureKinshipSum<MiniClosure>`
        // would emit references to `MiniClosure::path_count_column()` =
        // `"path_count"` and `MiniClosure::depth_column()` = `"depth"`,
        // but the join's ancestor_column / depth_column / etc. do not
        // match — so the gate fires.
        let mismatched_cp = ClosurePairJoin {
            table: "different_closure_table",
            source_column: "different_source",
            ancestor_column: "different_ancestor",
            depth_column: "different_depth",
            path_count_column: "different_path_count",
        };
        let sum: PairClosureKinshipSum<MiniClosure> = PairClosureKinshipSum::new();
        let err = crate::query::annotate::AnnotationSlot::validate_against_closure_pair(
            &sum,
            Some(&mismatched_cp),
        )
        .expect_err("mismatched aggregate-C vs join-C must be rejected");
        match err {
            DjogiError::Validation(msg) => {
                assert!(
                    msg.contains("ClosureModel mismatch"),
                    "validation message must name the mismatch: {msg}"
                );
                // The diagnostic should surface both the join's and the
                // aggregate's column-name views so adopters can see the
                // mismatch concretely.
                assert!(
                    msg.contains("different_path_count") && msg.contains("path_count"),
                    "diagnostic must surface join and aggregate column shapes: {msg}"
                );
            }
            other => panic!("expected DjogiError::Validation, got {other:?}"),
        }
    }

    // ── Blocker 2 — PK shape gate ────────────────────────────────────────

    /// Inert no-PK model — `PkType::None`. Identity-required paths must
    /// reject this at the terminal gate.
    struct NoPkModel;
    impl crate::model::__sealed::Sealed for NoPkModel {}
    #[allow(clippy::manual_async_fn)]
    impl crate::model::Model for NoPkModel {
        type Pk = HeerId;
        type Fields = ();
        fn table_name() -> &'static str {
            "no_pk_minis"
        }
        fn pk_value(&self) -> &Self::Pk {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            &NO_PK_MODEL_DESC
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
    impl FromPgRow for NoPkModel {
        const COLUMNS: &'static [&'static str] = &["name"];
        const COLUMN_LIST: &'static str = "name";
        fn from_pg_row(_row: &tokio_postgres::Row) -> Result<Self, DjogiError> {
            unreachable!()
        }
    }
    static NO_PK_MODEL_FIELDS: &[FieldDescriptor] =
        &[field_descriptor("name", FieldSqlType::Text, false)];
    static NO_PK_MODEL_DESC: ModelDescriptor =
        model_descriptor("NoPkModel", "no_pk_minis", PkType::None, NO_PK_MODEL_FIELDS);

    /// Inert composite-PK model — `PkType::Composite(&["a", "b"])`.
    /// Identity-required paths must reject this at the terminal gate
    /// because the v0.1.0 substrate emits single-column anti-equality
    /// / join keys / GROUP BY, not multi-column tuples.
    struct CompositePkModel;
    impl crate::model::__sealed::Sealed for CompositePkModel {}
    #[allow(clippy::manual_async_fn)]
    impl crate::model::Model for CompositePkModel {
        type Pk = HeerId;
        type Fields = ();
        fn table_name() -> &'static str {
            "composite_pk_minis"
        }
        fn pk_value(&self) -> &Self::Pk {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            &COMPOSITE_PK_MODEL_DESC
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
    impl FromPgRow for CompositePkModel {
        const COLUMNS: &'static [&'static str] = &["a", "b"];
        const COLUMN_LIST: &'static str = "a, b";
        fn from_pg_row(_row: &tokio_postgres::Row) -> Result<Self, DjogiError> {
            unreachable!()
        }
    }
    static COMPOSITE_PK_MODEL_FIELDS: &[FieldDescriptor] = &[
        field_descriptor("a", FieldSqlType::Text, false),
        field_descriptor("b", FieldSqlType::Text, false),
    ];
    static COMPOSITE_PK_COLS: &[&str] = &["a", "b"];
    static COMPOSITE_PK_MODEL_DESC: ModelDescriptor = model_descriptor(
        "CompositePkModel",
        "composite_pk_minis",
        PkType::Composite(COMPOSITE_PK_COLS),
        COMPOSITE_PK_MODEL_FIELDS,
    );

    #[test]
    fn validated_pk_column_for_pair_identity_accepts_simple_pks() {
        // Single-column PK shapes pass the validator and return
        // `id` (or whatever the descriptor names).
        let pk =
            validated_pk_column_for_pair_identity::<Mini>().expect("HeerId PK should validate");
        assert_eq!(pk, "id");
    }

    #[test]
    fn validated_pk_column_for_pair_identity_rejects_no_pk() {
        let err = validated_pk_column_for_pair_identity::<NoPkModel>()
            .expect_err("PkType::None must be rejected");
        match err {
            DjogiError::Validation(msg) => {
                assert!(msg.contains("no primary key") || msg.contains("PkType::None"));
                assert!(
                    msg.contains("no_pk_minis"),
                    "diagnostic must name the model table: {msg}"
                );
            }
            other => panic!("expected DjogiError::Validation, got {other:?}"),
        }
    }

    #[test]
    fn validated_pk_column_for_pair_identity_rejects_composite_pk() {
        let err = validated_pk_column_for_pair_identity::<CompositePkModel>()
            .expect_err("PkType::Composite must be rejected for this slice");
        match err {
            DjogiError::Validation(msg) => {
                assert!(
                    msg.contains("composite primary key"),
                    "diagnostic must mention composite PK: {msg}"
                );
                // Diagnostic should surface the columns so adopters can
                // see what would need multi-column emission.
                assert!(
                    msg.contains("\"a\"") && msg.contains("\"b\""),
                    "diagnostic must surface the composite columns: {msg}"
                );
            }
            other => panic!("expected DjogiError::Validation, got {other:?}"),
        }
    }

    #[test]
    fn validate_pair_identity_pk_short_circuits_when_not_needed() {
        // When no identity-required path is in scope, even no-PK models
        // are accepted — the validator returns `(None, None)` and the
        // emitters never reference a PK column.
        let result = validate_pair_identity_pk::<NoPkModel, CompositePkModel>(false)
            .expect("no-identity path accepts any PK shape");
        assert_eq!(result, (None, None));
    }

    #[test]
    fn validate_pair_identity_pk_validates_both_sides_when_needed() {
        // When identity is required, both sides are validated. A
        // single-column / no-pk pairing rejects on the no-pk side.
        let err = validate_pair_identity_pk::<Mini, NoPkModel>(true)
            .expect_err("right side has no PK; must reject");
        match err {
            DjogiError::Validation(msg) => {
                assert!(msg.contains("no primary key") || msg.contains("PkType::None"));
            }
            other => panic!("expected DjogiError::Validation, got {other:?}"),
        }
    }

    // ── Blocker 3 — Joined annotations reject ordinary AggregateExpr ─────

    #[test]
    fn aggregate_expr_is_not_joined_safe_by_default() {
        // Build a plain SUM(age) AggregateExpr<i64>. In joined
        // contexts it would emit `, SUM(age) OVER ()` referencing a
        // bare `age` column — ambiguous when both pair sides have
        // an `age` column (always true for self-joins).
        let age_ref: crate::query::FieldRef<Mini, i64> = crate::query::FieldRef::new("age");
        let agg: crate::expr::AggregateExpr<i64> = age_ref.sum();
        assert!(
            !crate::query::annotate::AnnotationSlot::is_joined_safe(&agg),
            "AggregateExpr<V> must not be joined-safe by default — it emits bare-column SQL"
        );
    }

    #[test]
    fn pair_closure_kinship_sum_is_joined_safe() {
        // The pair-tuple-native aggregate references closure-pair
        // aliases (`la.` / `ra.`) so there is no bare-column
        // ambiguity. It is the canonical joined-safe slot.
        let sum: PairClosureKinshipSum<MiniClosure> = PairClosureKinshipSum::new();
        assert!(
            crate::query::annotate::AnnotationSlot::is_joined_safe(&sum),
            "PairClosureKinshipSum must be joined-safe"
        );
    }

    #[test]
    fn window_function_slot_is_joined_safe() {
        // Window functions have pair-aware composition through
        // `PairWindowExt::partition_by_pair` so they are joined-safe
        // at the slot level. The composition's `&'static str` is
        // pair-side-qualified ("l.id" or "r.id") which keeps the
        // emitted SQL unambiguous.
        let win = crate::expr::RowNumber::new().alias("rank");
        assert!(
            crate::query::annotate::AnnotationSlot::is_joined_safe(&win),
            "RowNumber must be joined-safe (composes through PairWindowExt)"
        );
    }

    #[test]
    fn into_aggregate_tuple_is_joined_safe_ands_across_slots() {
        // Tuple impls AND across slots — one unsafe slot makes the
        // whole tuple unsafe.
        let age_ref: crate::query::FieldRef<Mini, i64> = crate::query::FieldRef::new("age");
        let safe_slot: PairClosureKinshipSum<MiniClosure> = PairClosureKinshipSum::new();
        let unsafe_slot: crate::expr::AggregateExpr<i64> = age_ref.sum();
        let tuple = (safe_slot, unsafe_slot);
        assert!(
            !crate::query::IntoAggregateTuple::is_joined_safe(&tuple),
            "tuple with an AggregateExpr slot must NOT be joined-safe"
        );
    }

    #[test]
    fn empty_tuple_is_joined_safe() {
        // `()` annotation is vacuously safe — no slots to emit.
        assert!(
            crate::query::IntoAggregateTuple::is_joined_safe(&()),
            "() annotation must be joined-safe"
        );
    }

    /// End-to-end gate: simulate the joined-annotated terminal's
    /// pre-build gate sequence — `check_legality` →
    /// `requires_closure_pair_join` → `validate_against_closure_pair`
    /// → `is_joined_safe` → `validate_pair_identity_pk` — for an
    /// ordinary `AggregateExpr` aggregate on a self-join queryset.
    ///
    /// Pins that `Animal::objects().self_pairs().annotate(|l, _|
    /// l.age().sum())` (or `Mini` in this fixture's case) reaches the
    /// `!is_joined_safe()` reject arm before any SQL is built.
    /// `JoinedAnnotatedQuerySet::fetch_all` is async-only, so the
    /// terminal itself is exercised by integration tests; this unit
    /// test pins the gate's specific decision against the same
    /// IntoAggregateTuple signals the async terminal consults.
    #[test]
    fn joined_annotate_rejects_ordinary_aggregate_expr_via_gate() {
        // Construct the same shape the user would build:
        //   Mini::objects().self_pairs().annotate(|l, _| l.age().sum())
        // (Mini's `Fields = ()` so we route through FieldRef directly.)
        let age_ref: crate::query::FieldRef<Mini, i64> = crate::query::FieldRef::new("age");
        let jqs: JoinedQuerySet<Mini, Mini> = QuerySet::<Mini>::new().self_pairs();
        let annotated = jqs.annotate(|_l, _r| age_ref.sum());

        // The terminal's gate sequence — emulated for sync test.
        // 1. check_legality must pass for the slot itself.
        assert!(annotated.aggregates.check_legality().is_ok());

        // 2. requires_closure_pair_join is false — ordinary aggregate
        //    doesn't need closure-pair join.
        assert!(!annotated.aggregates.requires_closure_pair_join());

        // 3. validate_against_closure_pair is Ok — ordinary aggregate
        //    carries no `C: ClosureModel` to validate.
        assert!(
            annotated
                .aggregates
                .validate_against_closure_pair(annotated.inner.closure_pair.as_ref())
                .is_ok()
        );

        // 4. is_joined_safe is false — this is the rejection signal.
        //    `JoinedAnnotatedQuerySet::fetch_all` consults exactly
        //    this in its gate and returns DjogiError::Validation.
        assert!(
            !annotated.aggregates.is_joined_safe(),
            "ordinary AggregateExpr in joined.annotate must trip the is_joined_safe gate"
        );
    }
}
