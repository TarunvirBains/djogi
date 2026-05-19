//! Annotation terminal — `QuerySet::annotate(...)` + its
//! [`AnnotatedQuerySet<T, A>`] pending handle.
//!
//! # What
//!
//! `QuerySet<T>::annotate(|f| f.col().sum())` returns an
//! [`AnnotatedQuerySet<T, A>`] whose terminal
//! [`AnnotatedQuerySet::fetch_all`] issues
//!
//! ```sql
//! SELECT t.<c1>, t.<c2>, ..., <agg_0> AS __djogi_agg_0, <agg_1> AS __djogi_agg_1, ...
//! FROM <table> AS t
//! [WHERE ...]
//! [ORDER BY ...]
//! [LIMIT $n] [OFFSET $n]
//! ```
//!
//! The `t.<c_N>` prefix is the canonical column list emitted by
//! `FromPgRow::COLUMN_LIST`, one column per field in struct-field
//! order. Using an explicit column list (rather than `t.*`) pins the
//! wire shape so `FromPgRow::from_pg_row` can decode positionally.
//!
//! and decodes the result into `Vec<(T, Decoded)>` where `Decoded`
//! is a single value for arity 1 and a tuple for arities 2..=4.
//!
//! # Why a typed-tuple return
//!
//! Per Phase 4 plan Q5 (Resolved: A + doc-only guidance), annotations
//! return `Vec<(Model, Aggregates...)>`. Users who need 3+ aggregates
//! either stay on the tuple form or shape into a local struct post-
//! hoc — there is no const-generic lint discouraging wide tuples.
//!
//! # The sealed `IntoAggregateTuple` trait
//!
//! Implemented for:
//!
//! - `AggregateExpr<V, K>` (arity 1) — `Decoded = V`
//! - `(AggregateExpr<V1, K1>, AggregateExpr<V2, K2>)` — `Decoded = (V1, V2)`
//! - `(AggregateExpr<V1, K1>, AggregateExpr<V2, K2>, AggregateExpr<V3, K3>)`
//!   — `Decoded = (V1, V2, V3)`
//! - `(AggregateExpr<V1, K1>, AggregateExpr<V2, K2>, AggregateExpr<V3, K3>,
//!   AggregateExpr<V4, K4>)` — `Decoded = (V1, V2, V3, V4)`
//!
//! Plain ungrouped [`QuerySet::annotate`] applies the narrower
//! [`PlainAnnotationTuple`] bound because that terminal synthesizes
//! `OVER ()` for aggregate slots. Only value aggregates can use that
//! synthesized window. Ordered-set, hypothetical-set, and metadata
//! aggregate kinds remain legal through scalar [`QuerySet::aggregate`]
//! and grouped annotate, but do not compile on the plain annotate path.
//!
//! The trait is sealed by a private supertrait so downstream crates
//! cannot add their own impls — same seal pattern as
//! [`crate::query::queryset::IntoDistinctColumns`].
//!
//! # Why ordinal-prefix decode for `T` + name-based decode for aggregates
//!
//! The main row contains both `T`'s columns (emitted as
//! `t.<c1>, t.<c2>, ...` in the canonical `FromPgRow::COLUMNS`
//! order) and the aggregate columns (from `<agg> AS __djogi_agg_N`).
//! `FromPgRow::from_pg_row` decodes positions `0..N_COLS` and its
//! column-count assert is `>= N_COLS`, so the trailing aggregate
//! columns do not interfere with the model decode. The aggregate
//! tuple then reads each slot's value by the `__djogi_agg_N` alias
//! directly from the same `Row` — name-based because the aliases
//! are stable well-known strings that never clash with model
//! columns.

#![allow(clippy::manual_async_fn)]

use crate::DjogiError;
use crate::context::DjogiContext;
use crate::expr::{
    AggregateExpr, DenseRank, FirstValueWindow, LagWindow, LastValueWindow, LeadWindow,
    NthValueWindow, Rank, RowNumber,
    aggregate::{KindEvidence, ValueAgg},
};
use crate::model::Model;
use crate::pg::accumulator::{SqlAccumulator, as_params};
use crate::pg::decode::FromPgRow;
use crate::query::queryset::QuerySet;
use crate::query::sql::{build_annotated_select_for_fetch, emit_aggregate_with_window_and_cast};
use std::future::Future;
use std::marker::PhantomData;

// ── Sealed seal ──────────────────────────────────────────────────────

// Sealed marker for [`IntoAggregateTuple`] impls.
//
// Phase 8.5 Cluster 4A widens this from `mod` to `pub(crate) mod` so
// pair-tuple slot types implemented in `query::joined` (which compose
// through the existing `impl<S: AnnotationSlot> IntoAggregateTuple for
// S` blanket) can satisfy the bound through `AnnotationSlot::Sealed`.
// `Sealed` remains un-namable outside `djogi` because the module is
// `pub(crate)`.
pub(crate) mod sealed {
    //! Crate-private seal. Downstream code cannot name
    //! `sealed::Sealed`, so the only `IntoAggregateTuple` impls are
    //! the framework-blessed ones below.
    pub trait Sealed {}
}

// Sealed marker for [`AnnotationSlot`] impls.
//
// Phase 8.5 Cluster 4A widens the inner module from `mod` to
// `pub(crate) mod` so the pair-tuple surface (`query::joined`) can
// implement `AnnotationSlot` for `PairClosureKinshipSum<C>` and the
// blanket `impl<S: AnnotationSlot> IntoAggregateTuple for S` picks up
// the new slot without weakening the seal against downstream crates
// — `Sealed` stays un-namable outside `djogi` because the module is
// `pub(crate)`.
pub(crate) mod annotation_slot_sealed {
    pub trait Sealed {}
}

/// Framework-reserved column alias for the Nth slot in an annotate-tuple
/// emission. Bounded to slot < 4 by the four `impl_into_aggregate_tuple!`
/// invocations in this module — exceeding that bound is a framework-internal
/// invariant break, surfaced through the explicit `unreachable!` panic so a
/// future regression has a self-explaining diagnostic instead of an
/// `index out of bounds` error.
fn aggregate_alias(slot: usize) -> &'static str {
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

/// A single annotation expression that can occupy one SELECT-list slot.
///
/// This is a hidden extension point used by Djogi's own annotation tuple
/// bridge. It is sealed, so downstream crates cannot inject custom SQL into
/// the annotation emitter. Public code normally names
/// [`IntoAggregateTuple`], not this trait.
#[doc(hidden)]
pub trait AnnotationSlot: annotation_slot_sealed::Sealed {
    /// Rust value decoded from this annotation's SELECT-list slot.
    type Decoded;

    /// Push this annotation as `, <expr> AS <alias>` for ungrouped annotate.
    fn push_column(&self, acc: &mut SqlAccumulator, slot: usize);

    /// Push this annotation in grouped annotate contexts.
    fn push_column_bare(&self, acc: &mut SqlAccumulator, slot: usize);

    /// Push this annotation in grouped annotate contexts, choosing whether
    /// this slot needs a leading SELECT-list separator.
    fn push_column_bare_after(
        &self,
        acc: &mut SqlAccumulator,
        slot: usize,
        has_previous_columns: bool,
    );

    /// Decode this annotation from `row`.
    fn decode_column(
        &self,
        row: &tokio_postgres::Row,
        slot: usize,
    ) -> Result<Self::Decoded, tokio_postgres::Error>;

    /// Validate runtime invariants before SQL emission.
    fn check_legality(&self) -> Result<(), crate::DjogiError> {
        Ok(())
    }

    /// Whether this annotation slot needs a closure-pair LEFT JOIN
    /// (i.e. `la` / `ra` aliases) to be in scope to produce valid SQL.
    ///
    /// Returns `false` by default — virtually every annotation slot is
    /// independent of a pair-tuple's closure join. The single override
    /// today is [`PairClosureKinshipSum`](crate::query::PairClosureKinshipSum):
    /// it emits SQL that references `la.<path_count>` / `ra.<depth>` so
    /// without a closure-pair join in the FROM clause it produces a
    /// Postgres `42P01 missing FROM-clause` error at execute time.
    ///
    /// [`JoinedAnnotatedQuerySet::fetch_all`](crate::query::JoinedAnnotatedQuerySet::fetch_all)
    /// consults this through the tuple bridge
    /// ([`IntoAggregateTuple::requires_closure_pair_join`]) before SQL
    /// build and returns a typed `DjogiError::Validation` when the
    /// aggregate tuple needs the join but the queryset has no
    /// `closure_pair` set, replacing the cryptic Postgres error with a
    /// pre-build diagnostic.
    fn requires_closure_pair_join(&self) -> bool {
        false
    }

    /// Whether this slot's emitted SQL is safe to use inside a
    /// pair-tuple `JoinedQuerySet::annotate(...)` terminal.
    ///
    /// Returns `false` by default. Override to `true` only when the
    /// slot's emitter either:
    ///   - References pair-side closure aliases (`la.` / `ra.`)
    ///     explicitly — `PairClosureKinshipSum<C>`.
    ///   - Composes through pair-aware builders that qualify column
    ///     references (`l.col` / `r.col`) — window functions invoked
    ///     via `partition_by_pair` / `order_by_pair_asc` /
    ///     `order_by_pair_desc`.
    ///
    /// Ordinary [`AggregateExpr<V>`] (e.g. `f.age().sum()`) keeps
    /// the default `false`: it emits a bare-column SQL fragment like
    /// `SUM(age) OVER ()` which is structurally ambiguous in joined
    /// contexts where both sides may share column names (a guarantee
    /// on self-joins, typical on heterogeneous joins).
    ///
    /// [`JoinedAnnotatedQuerySet::fetch_all`](crate::query::JoinedAnnotatedQuerySet::fetch_all)
    /// consults this through the tuple bridge
    /// ([`IntoAggregateTuple::is_joined_safe`]) before SQL build and
    /// rejects the entire tuple with a typed `DjogiError::Validation`
    /// when any slot is not joined-safe. The diagnostic message points
    /// adopters at the pair-aware alternatives.
    ///
    /// Note: window functions return `true` here even if the user
    /// composes them with the non-pair `partition_by(field)` instead
    /// of `partition_by_pair(side, field)` — that would still emit a
    /// bare column reference. Detecting that variant requires
    /// inspecting the `WindowSpec.partition_by` / `.order_by` strings
    /// for `.`-qualification; a follow-on slice will tighten the gate.
    /// For now, the slot-level opt-in keeps the surface minimal while
    /// the ambiguous case (`AggregateExpr` with no pair-aware path)
    /// stays rejected.
    fn is_joined_safe(&self) -> bool {
        false
    }

    /// Whether this slot's emitted SQL is **only** valid inside a
    /// pair-tuple `JoinedQuerySet::annotate(...)` terminal — i.e.,
    /// references the framework-fixed `l.` / `r.` / `la.` / `ra.`
    /// aliases that a single-Model or grouped FROM clause does not
    /// provide.
    ///
    /// Returns `false` by default. Override to `true` when the slot's
    /// SQL emitter splices a pair-tuple alias literally:
    ///
    ///   - [`PairClosureKinshipSum<C>`](crate::query::PairClosureKinshipSum)
    ///     emits `SUM(la.path_count * ra.path_count * ...)` referencing
    ///     the closure-pair `la` / `ra` aliases. Already covered by the
    ///     existing `requires_closure_pair_join()` signal, but pair-tuple
    ///     scope is the broader gate (this slot needs both pair-side
    ///     aliases **and** the closure-pair LEFT JOINs).
    ///   - [`PairAreaOverlapRatio<L, R>`](crate::query::PairAreaOverlapRatio)
    ///     (spatial only) emits
    ///     `ST_Area(ST_Intersection(l.<lcol>, r.<rcol>))` referencing
    ///     the pair-side `l` / `r` aliases. Does **not** need closure
    ///     pair joins, so the existing `requires_closure_pair_join()`
    ///     signal alone would let it sneak past the single-Model /
    ///     grouped annotate gates. This is the signal that catches it.
    ///
    /// [`AnnotatedQuerySet::fetch_all`](crate::query::AnnotatedQuerySet::fetch_all)
    /// and the grouped annotate terminals consult this through the tuple
    /// bridge ([`IntoAggregateTuple::requires_pair_tuple_scope`]) before
    /// SQL build and return a typed `DjogiError::Validation` when the
    /// aggregate tuple includes any pair-only slot. The diagnostic
    /// points adopters at the `self_pairs()` / `cross_join_with()`
    /// entry points for the pair-tuple substrate.
    ///
    /// # Why this is separate from `requires_closure_pair_join`
    ///
    /// `requires_closure_pair_join()` is the narrower signal: it asks
    /// whether the slot's SQL references the **closure-pair** `la.` /
    /// `ra.` aliases specifically. A slot that needs only the pair-side
    /// `l.` / `r.` aliases (without closure metadata) does not require
    /// a `left_join_closure_pair::<C>()` and reports `false` to that
    /// gate. Without the distinct `requires_pair_tuple_scope()` signal,
    /// such a slot (today: `PairAreaOverlapRatio`) would compile in a
    /// single-Model `QuerySet::annotate(...)` chain and fail at Postgres
    /// with `42P01 missing FROM-clause entry for table "l"` at execute
    /// time. The two-signal design keeps the failure modes typed.
    fn requires_pair_tuple_scope(&self) -> bool {
        false
    }

    /// Validate this slot against an optional closure-pair join
    /// captured by the queryset.
    ///
    /// Default returns `Ok(())`. Override on slots that splice
    /// `ClosureModel` metadata (column-name `&'static str` accessors)
    /// into emitted SQL — today only
    /// [`PairClosureKinshipSum<C>`](crate::query::PairClosureKinshipSum)
    /// fits — to:
    ///   (1) Run [`crate::query::closure::validate_closure_metadata_idents::<C>`]
    ///       so a hand-rolled `impl ClosureModel` cannot smuggle SQL
    ///       through the slot's `push_sql` sites.
    ///   (2) When `closure_pair` is `Some(cp)`, compare each captured
    ///       identifier on `cp` against `C`'s same-named accessor so
    ///       a `left_join_closure_pair::<C1>()` paired with
    ///       `PairClosureKinshipSum::<C2>::new()` (C1 ≠ C2) is
    ///       rejected before SQL build instead of surfacing as a
    ///       Postgres `42703 column does not exist` error.
    ///
    /// [`JoinedAnnotatedQuerySet::fetch_all`](crate::query::JoinedAnnotatedQuerySet::fetch_all)
    /// calls this through the tuple bridge
    /// ([`IntoAggregateTuple::validate_against_closure_pair`]).
    fn validate_against_closure_pair(
        &self,
        #[allow(unused_variables)] closure_pair: Option<&crate::query::joined::ClosurePairJoin>,
    ) -> Result<(), crate::DjogiError> {
        Ok(())
    }
}

/// A single annotation slot that is allowed to flow through the plain
/// ungrouped `QuerySet::annotate` SELECT-list emitter.
///
/// This is intentionally narrower than [`AnnotationSlot`]. `AnnotationSlot`
/// remains the shared grouped/scalar annotation bridge; `PlainAnnotationSlot`
/// protects the ungrouped path that calls `push_column` and therefore may add
/// a synthesized `OVER ()`.
#[doc(hidden)]
pub trait PlainAnnotationSlot: AnnotationSlot {}

/// Tuple-level counterpart to [`PlainAnnotationSlot`].
///
/// `QuerySet::annotate` is the only public terminal builder that requires this
/// bound. Grouped annotate continues to require only [`IntoAggregateTuple`] so
/// metadata, ordered-set, and hypothetical-set aggregates remain available in
/// grouped contexts where no synthesized `OVER ()` is added.
#[doc(hidden)]
pub trait PlainAnnotationTuple: IntoAggregateTuple {
    /// Push plain-annotate SELECT-list columns. Implemented only for tuple
    /// shapes whose slots are legal on the synthesized-window path.
    fn push_plain_columns(&self, acc: &mut SqlAccumulator);
}

/// Type-level bridge from the closure return type of
/// [`QuerySet::annotate`] to the SELECT-list + row-decode logic.
///
/// Sealed via [`sealed::Sealed`] — downstream crates can name the
/// trait as a bound (`where A: IntoAggregateTuple`) but cannot
/// implement it. Every impl is framework-provided, which keeps the
/// SELECT-list shape and decode ordering in lockstep across all
/// supported arities.
///
/// `Decoded` is the tuple type users receive at fetch time: the scalar
/// for arity 1, a Rust tuple for arities 2..=4, or `()` for the explicitly
/// empty annotation edge case.
pub trait IntoAggregateTuple: sealed::Sealed {
    /// Rust tuple type returned inside `Vec<(T, Decoded)>`.
    type Decoded;

    /// Push annotation SELECT-list columns onto `acc`, each prefixed with
    /// `, ` and aliased for row decode.
    ///
    /// Indexing starts at 0; callers already pushed `SELECT t.*` so
    /// every push here begins with a comma.
    fn push_columns(&self, acc: &mut SqlAccumulator);

    /// Push the aggregate SELECT-list columns onto `acc` without the
    /// `OVER ()` window-function wrap, each aliased as `__djogi_agg_{N}`.
    ///
    /// Used by `build_grouped_annotated_select` — a `GROUP BY` query must
    /// not use window functions in the SELECT list for its aggregate columns.
    /// Every push here begins with a comma; callers already pushed the key
    /// columns as the leading SELECT columns.
    fn push_columns_bare(&self, acc: &mut SqlAccumulator);

    /// Push grouped aggregate SELECT-list columns after an optional key list.
    ///
    /// `group_by_sets` uses the unit key `()`, so it has no typed key columns
    /// to emit before the aggregate list. In that shape the first aggregate
    /// must not prepend `, `.
    fn push_columns_bare_after(&self, acc: &mut SqlAccumulator, has_previous_columns: bool);

    /// Decode the annotation columns from `row` into [`Self::Decoded`].
    ///
    /// The aliases are fixed (`__djogi_agg_0`, `__djogi_agg_1`, ...)
    /// so the impl reads each slot by name. Offset-based decoding is
    /// not used because `row.try_get` by name is more robust to
    /// column-ordering surprises (though the framework never reorders
    /// the SELECT list in practice).
    fn decode_tuple(
        &self,
        row: &tokio_postgres::Row,
    ) -> Result<Self::Decoded, tokio_postgres::Error>;

    /// Number of SELECT-list annotation slots this value contributes.
    fn annotation_count(&self) -> usize;

    /// Validate all aggregate nodes in this tuple for unsupported
    /// DISTINCT modifier combinations before building SQL.
    ///
    /// Called at the start of each terminal method
    /// ([`AnnotatedQuerySet::fetch_all`], grouped terminals) so the
    /// caller gets a typed [`crate::DjogiError::UnsupportedAggregate`]
    /// rather than a cryptic Postgres syntax error.
    ///
    /// Default impl returns `Ok(())` — overridden by each concrete impl
    /// to walk its nodes through
    /// [`crate::expr::sql::check_aggregate_legality`].
    fn check_legality(&self) -> Result<(), crate::DjogiError> {
        Ok(())
    }

    /// Whether any slot in this aggregate tuple requires a closure-pair
    /// LEFT JOIN to be in scope to produce valid SQL.
    ///
    /// Single-slot impls (the blanket `impl<S> IntoAggregateTuple for S
    /// where S: AnnotationSlot`) forward to
    /// [`AnnotationSlot::requires_closure_pair_join`]; tuple impls OR
    /// across every slot. Default `false` covers the `()` /
    /// no-annotation case.
    ///
    /// [`JoinedAnnotatedQuerySet::fetch_all`](crate::query::JoinedAnnotatedQuerySet::fetch_all)
    /// queries this before SQL build so the cluster-4A
    /// `PairClosureKinshipSum` slot is rejected with a typed
    /// `DjogiError::Validation` when the queryset has no `closure_pair`
    /// set, instead of letting Postgres surface a missing-FROM-clause
    /// error.
    fn requires_closure_pair_join(&self) -> bool {
        false
    }

    /// Whether **every** slot in this aggregate tuple is safe to use
    /// inside a pair-tuple `JoinedQuerySet::annotate(...)` terminal.
    ///
    /// Forwards to [`AnnotationSlot::is_joined_safe`] for the single-
    /// slot blanket; tuple impls AND across slots. Default `true`
    /// covers the `()` / no-annotation case (vacuously safe).
    ///
    /// [`JoinedAnnotatedQuerySet::fetch_all`](crate::query::JoinedAnnotatedQuerySet::fetch_all)
    /// queries this before SQL build to reject ordinary
    /// `AggregateExpr<V>` slots (e.g. `f.age().sum()`) that would emit
    /// bare-column SQL like `SUM(age) OVER ()` — ambiguous on
    /// self-joins where both sides share columns.
    fn is_joined_safe(&self) -> bool {
        true
    }

    /// Whether any slot in this aggregate tuple requires pair-tuple
    /// scope (`l.` / `r.` / `la.` / `ra.` aliases) to produce valid
    /// SQL.
    ///
    /// Forwards to [`AnnotationSlot::requires_pair_tuple_scope`] for
    /// the single-slot blanket; tuple impls OR across every slot.
    /// Default `false` covers the `()` / no-annotation case.
    ///
    /// [`AnnotatedQuerySet::fetch_all`](crate::query::AnnotatedQuerySet::fetch_all)
    /// and the grouped annotate terminals consult this before SQL
    /// build so pair-only slots (`PairClosureKinshipSum`,
    /// `PairAreaOverlapRatio`) are rejected with a typed
    /// `DjogiError::Validation` instead of letting Postgres surface a
    /// `42P01 missing FROM-clause entry for table "l"` error at
    /// execute time. The diagnostic points adopters at the
    /// `self_pairs()` / `cross_join_with()` entry points.
    fn requires_pair_tuple_scope(&self) -> bool {
        false
    }

    /// Validate every slot in this aggregate tuple against the
    /// queryset's optional closure-pair join. Forwards to
    /// [`AnnotationSlot::validate_against_closure_pair`] per slot
    /// (single-slot blanket calls once; tuple impls walk every slot
    /// and propagate the first error).
    ///
    /// Default returns `Ok(())` — `()` and tuples-of-no-pair-aggregates
    /// trivially validate.
    ///
    /// [`JoinedAnnotatedQuerySet::fetch_all`](crate::query::JoinedAnnotatedQuerySet::fetch_all)
    /// calls this before SQL build so a `PairClosureKinshipSum<C>`
    /// slot whose `C`'s metadata is hostile or mismatched against the
    /// join's `C` surfaces as a typed `DjogiError::Validation` before
    /// any SQL is emitted.
    fn validate_against_closure_pair(
        &self,
        #[allow(unused_variables)] closure_pair: Option<&crate::query::joined::ClosurePairJoin>,
    ) -> Result<(), crate::DjogiError> {
        Ok(())
    }
}

// ── Single annotation slots ──────────────────────────────────────────

// Plain ungrouped aggregate annotations are emitted with `OVER ()` so the
// annotate SELECT-list stays valid without a `GROUP BY` clause. That default
// window is valid only for value aggregates; `PlainAnnotationSlot` is
// implemented for `AggregateExpr<_, ValueAgg>` but not for metadata,
// ordered-set, or hypothetical-set kinds. The broader `AnnotationSlot` impl
// remains generic so scalar aggregate and grouped annotate can still use every
// aggregate kind without a synthesized window.

impl<V, K> annotation_slot_sealed::Sealed for AggregateExpr<V, K> {}
impl<V, K> AnnotationSlot for AggregateExpr<V, K>
where
    K: KindEvidence,
    V: for<'a> postgres_types::FromSql<'a> + Send + Unpin + 'static,
{
    type Decoded = V;

    fn push_column(&self, acc: &mut SqlAccumulator, slot: usize) {
        acc.push_sql(", ");
        // Phase 8eta PR2b: `emit_aggregate_*` is now fallible because
        // aggregates may carry filter expressions that contain portable
        // subqueries. The annotation-slot trait surface is `()`-returning
        // (changing it would cascade through every aggregate + window
        // tuple impl), so we `.expect` here. In practice this path only
        // sees aggregates over scalar columns — the failure modes
        // require a nested subquery filter with a malformed portable
        // predicate, which the type system rejects upstream. PR2c may
        // make the trait surface Result-returning if real-world adopter
        // code surfaces this edge case.
        emit_aggregate_with_window_and_cast(acc, &self.node)
            .expect("aggregate annotation emission cannot fail for typed-aggregate inputs");
        acc.push_sql(" AS ");
        acc.push_sql(aggregate_alias(slot));
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
        crate::query::sql::emit_aggregate_with_cast(acc, &self.node)
            .expect("aggregate annotation emission cannot fail for typed-aggregate inputs");
        acc.push_sql(" AS ");
        acc.push_sql(aggregate_alias(slot));
    }

    fn decode_column(
        &self,
        row: &tokio_postgres::Row,
        slot: usize,
    ) -> Result<Self::Decoded, tokio_postgres::Error> {
        row.try_get::<_, V>(aggregate_alias(slot))
    }

    fn check_legality(&self) -> Result<(), crate::DjogiError> {
        crate::expr::sql::check_aggregate_legality(&self.node)
    }
}

impl<V> PlainAnnotationSlot for AggregateExpr<V, ValueAgg> where
    V: for<'a> postgres_types::FromSql<'a> + Send + Unpin + 'static
{
}

macro_rules! impl_window_annotation_slot {
    ($type_name:ty, $display:literal) => {
        impl_window_annotation_slot!($type_name, $display, decoded = i64);
    };
    ($type_name:ty, $display:literal, decoded = $decoded:ty) => {
        impl annotation_slot_sealed::Sealed for $type_name {}

        impl AnnotationSlot for $type_name {
            type Decoded = $decoded;

            fn push_column(&self, acc: &mut SqlAccumulator, _slot: usize) {
                acc.push_sql(", ");
                self.push_annotated_column(acc);
            }

            fn push_column_bare(&self, acc: &mut SqlAccumulator, slot: usize) {
                self.push_column_bare_after(acc, slot, true);
            }

            fn push_column_bare_after(
                &self,
                acc: &mut SqlAccumulator,
                _slot: usize,
                has_previous_columns: bool,
            ) {
                if has_previous_columns {
                    acc.push_sql(", ");
                }
                self.push_annotated_column(acc);
            }

            fn decode_column(
                &self,
                row: &tokio_postgres::Row,
                _slot: usize,
            ) -> Result<Self::Decoded, tokio_postgres::Error> {
                row.try_get::<_, $decoded>(
                    self.alias_name()
                        .expect("window function annotations are checked before row decode"),
                )
            }

            fn check_legality(&self) -> Result<(), crate::DjogiError> {
                if self.alias_name().is_some() {
                    Ok(())
                } else {
                    Err(crate::DjogiError::Validation(format!(
                        "{} window annotation requires .alias(\"name\") before annotate",
                        $display
                    )))
                }
            }

            // Rank-family windows (`ROW_NUMBER`, `RANK`, `DENSE_RANK`)
            // have pair-aware composition through
            // [`PairWindowExt`](crate::query::joined::PairWindowExt) —
            // `partition_by_pair` / `order_by_pair_asc` /
            // `order_by_pair_desc` qualify each stored column with the
            // side's alias (`"l.<col>"` / `"r.<col>"`). The instance is
            // joined-safe iff every stored column is so qualified
            // (vacuously safe when there is no PARTITION BY / ORDER BY,
            // since `ROW_NUMBER() OVER ()` references no columns).
            //
            // A user-built `RowNumber::new().partition_by(f.id())` —
            // the non-pair-aware path — stores the bare `"id"` and
            // trips this gate, getting rejected at
            // [`JoinedAnnotatedQuerySet::fetch_all`](crate::query::JoinedAnnotatedQuerySet::fetch_all)
            // before SQL build instead of silently emitting
            // `PARTITION BY id` against a `FROM animals AS l CROSS JOIN
            // animals AS r` where `id` is ambiguous.
            fn is_joined_safe(&self) -> bool {
                self.window.is_pair_qualified()
            }
        }

        impl PlainAnnotationSlot for $type_name {}
    };
}

/// Generic-V version for column-argument window functions: `FIRST_VALUE`,
/// `LAST_VALUE`, `LEAD`, `LAG`, `NTH_VALUE`. Each type carries a phantom
/// `V` for the decoded column type; the impl block decodes the row into
/// `V` directly.
macro_rules! impl_window_annotation_slot_generic_v {
    ($type_name:ident, $display:literal) => {
        impl<V> annotation_slot_sealed::Sealed for $type_name<V> {}

        impl<V> AnnotationSlot for $type_name<V>
        where
            V: for<'a> postgres_types::FromSql<'a> + Send + Unpin + 'static,
        {
            type Decoded = V;

            fn push_column(&self, acc: &mut SqlAccumulator, _slot: usize) {
                acc.push_sql(", ");
                self.push_annotated_column(acc);
            }

            fn push_column_bare(&self, acc: &mut SqlAccumulator, slot: usize) {
                self.push_column_bare_after(acc, slot, true);
            }

            fn push_column_bare_after(
                &self,
                acc: &mut SqlAccumulator,
                _slot: usize,
                has_previous_columns: bool,
            ) {
                if has_previous_columns {
                    acc.push_sql(", ");
                }
                self.push_annotated_column(acc);
            }

            fn decode_column(
                &self,
                row: &tokio_postgres::Row,
                _slot: usize,
            ) -> Result<Self::Decoded, tokio_postgres::Error> {
                row.try_get::<_, V>(
                    self.alias_name()
                        .expect("window function annotations are checked before row decode"),
                )
            }

            fn check_legality(&self) -> Result<(), crate::DjogiError> {
                if self.alias_name().is_some() {
                    Ok(())
                } else {
                    Err(crate::DjogiError::Validation(format!(
                        "{} window annotation requires .alias(\"name\") before annotate",
                        $display
                    )))
                }
            }

            // Column-argument window functions (`FIRST_VALUE`,
            // `LAST_VALUE`, `LEAD`, `LAG`, `NTH_VALUE`) carry a
            // `target_column` constructed via
            // `target.into_sql_field().column()` — always a bare
            // column name validated by
            // [`crate::ident::assert_plain_ident`]. There is no
            // pair-aware constructor that would qualify the target as
            // `"l.<col>"` / `"r.<col>"`, so the emitted SQL would be
            // `FIRST_VALUE(name) OVER (...)` — ambiguous in self-join
            // contexts where both pair sides carry a `name` column.
            //
            // Reporting `false` here makes
            // [`JoinedAnnotatedQuerySet::fetch_all`](crate::query::JoinedAnnotatedQuerySet::fetch_all)
            // reject the slot at the safety gate with a typed
            // `DjogiError::Validation`. A pair-aware constructor (e.g.
            // `FirstValueWindow::new_pair(PairSide::Left, ...)`) that
            // composes the side alias into `target_column` is tracked
            // as a follow-up slice — see `docs/spec/`.
            fn is_joined_safe(&self) -> bool {
                false
            }
        }

        impl<V> PlainAnnotationSlot for $type_name<V> where
            V: for<'a> postgres_types::FromSql<'a> + Send + Unpin + 'static
        {
        }
    };
}

impl_window_annotation_slot!(RowNumber, "RowNumber");
impl_window_annotation_slot!(Rank, "Rank");
impl_window_annotation_slot!(DenseRank, "DenseRank");
// Cluster E T19 — zero-arg returning f64
impl_window_annotation_slot!(
    crate::expr::PercentRankWindow,
    "PercentRankWindow",
    decoded = f64
);
impl_window_annotation_slot!(crate::expr::CumeDistWindow, "CumeDistWindow", decoded = f64);
// Cluster E T19 — single-integer-arg returning i32
impl_window_annotation_slot!(crate::expr::NtileWindow, "NtileWindow", decoded = i32);
// Cluster E T18 — column-arg generic V
impl_window_annotation_slot_generic_v!(FirstValueWindow, "FirstValueWindow");
impl_window_annotation_slot_generic_v!(LastValueWindow, "LastValueWindow");
impl_window_annotation_slot_generic_v!(LeadWindow, "LeadWindow");
impl_window_annotation_slot_generic_v!(LagWindow, "LagWindow");
impl_window_annotation_slot_generic_v!(NthValueWindow, "NthValueWindow");

impl<S> sealed::Sealed for S where S: AnnotationSlot {}

impl<S> IntoAggregateTuple for S
where
    S: AnnotationSlot,
{
    type Decoded = <S as AnnotationSlot>::Decoded;

    fn push_columns(&self, acc: &mut SqlAccumulator) {
        self.push_column(acc, 0);
    }

    fn push_columns_bare(&self, acc: &mut SqlAccumulator) {
        self.push_columns_bare_after(acc, true);
    }

    fn push_columns_bare_after(&self, acc: &mut SqlAccumulator, has_previous_columns: bool) {
        self.push_column_bare_after(acc, 0, has_previous_columns);
    }

    fn decode_tuple(
        &self,
        row: &tokio_postgres::Row,
    ) -> Result<Self::Decoded, tokio_postgres::Error> {
        self.decode_column(row, 0)
    }

    fn annotation_count(&self) -> usize {
        1
    }

    fn check_legality(&self) -> Result<(), crate::DjogiError> {
        AnnotationSlot::check_legality(self)
    }

    fn requires_closure_pair_join(&self) -> bool {
        AnnotationSlot::requires_closure_pair_join(self)
    }

    fn is_joined_safe(&self) -> bool {
        AnnotationSlot::is_joined_safe(self)
    }

    fn requires_pair_tuple_scope(&self) -> bool {
        AnnotationSlot::requires_pair_tuple_scope(self)
    }

    fn validate_against_closure_pair(
        &self,
        closure_pair: Option<&crate::query::joined::ClosurePairJoin>,
    ) -> Result<(), crate::DjogiError> {
        AnnotationSlot::validate_against_closure_pair(self, closure_pair)
    }
}

impl<S> PlainAnnotationTuple for S
where
    S: PlainAnnotationSlot,
{
    fn push_plain_columns(&self, acc: &mut SqlAccumulator) {
        self.push_column(acc, 0);
    }
}

impl sealed::Sealed for () {}

impl IntoAggregateTuple for () {
    type Decoded = ();

    fn push_columns(&self, _acc: &mut SqlAccumulator) {}

    fn push_columns_bare(&self, _acc: &mut SqlAccumulator) {}

    fn push_columns_bare_after(&self, _acc: &mut SqlAccumulator, _has_previous_columns: bool) {}

    fn decode_tuple(
        &self,
        _row: &tokio_postgres::Row,
    ) -> Result<Self::Decoded, tokio_postgres::Error> {
        Ok(())
    }

    fn annotation_count(&self) -> usize {
        0
    }
}

impl PlainAnnotationTuple for () {
    fn push_plain_columns(&self, _acc: &mut SqlAccumulator) {}
}

// ── Arity 2..=4: tuples of annotation slots ──────────────────────────
//
// One macro invocation per arity. The macro generates the sealed
// marker, the `push_columns` body that walks the tuple emitting each
// slot, and the `decode_tuple` body that reads each slot by its
// `__djogi_agg_N` alias. Arity > 4 is intentionally unsupported —
// per plan Q5 the rustdoc steers users to a local struct for wider
// shapes.

macro_rules! impl_into_aggregate_tuple {
    (
        arity = $arity:tt,
        types = [ $( ($ty:ident, $slot:tt) ),+ $(,)? ]
    ) => {
        impl<$($ty),+> sealed::Sealed for ( $($ty,)+ )
        where
            $($ty: AnnotationSlot,)+
        {}

        impl<$($ty),+> IntoAggregateTuple for ( $($ty,)+ )
        where
            $($ty: AnnotationSlot,)+
        {
            type Decoded = ( $(<$ty as AnnotationSlot>::Decoded,)+ );

            fn push_columns(&self, acc: &mut SqlAccumulator) {
                $(
                    self.$slot.push_column(acc, $slot);
                )+
            }

            fn push_columns_bare(&self, acc: &mut SqlAccumulator) {
                self.push_columns_bare_after(acc, true);
            }

            fn push_columns_bare_after(&self, acc: &mut SqlAccumulator, has_previous_columns: bool) {
                $(
                    self.$slot.push_column_bare_after(acc, $slot, has_previous_columns || $slot > 0);
                )+
            }

            fn decode_tuple(&self, row: &tokio_postgres::Row) -> Result<Self::Decoded, tokio_postgres::Error> {
                Ok((
                    $(
                        self.$slot.decode_column(row, $slot)?,
                    )+
                ))
            }

            fn annotation_count(&self) -> usize {
                $arity
            }

            fn check_legality(&self) -> Result<(), crate::DjogiError> {
                $(
                    self.$slot.check_legality()?;
                )+
                Ok(())
            }

            fn requires_closure_pair_join(&self) -> bool {
                false $(
                    || self.$slot.requires_closure_pair_join()
                )+
            }

            fn is_joined_safe(&self) -> bool {
                // AND across slots: every slot must be joined-safe.
                true $(
                    && self.$slot.is_joined_safe()
                )+
            }

            fn requires_pair_tuple_scope(&self) -> bool {
                // OR across slots: any pair-only slot poisons the
                // entire tuple for the single-Model / grouped paths.
                false $(
                    || self.$slot.requires_pair_tuple_scope()
                )+
            }

            fn validate_against_closure_pair(
                &self,
                closure_pair: Option<&crate::query::joined::ClosurePairJoin>,
            ) -> Result<(), crate::DjogiError> {
                $(
                    self.$slot.validate_against_closure_pair(closure_pair)?;
                )+
                Ok(())
            }
        }

        impl<$($ty),+> PlainAnnotationTuple for ( $($ty,)+ )
        where
            $($ty: PlainAnnotationSlot,)+
        {
            fn push_plain_columns(&self, acc: &mut SqlAccumulator) {
                $(
                    self.$slot.push_column(acc, $slot);
                )+
            }
        }
    };
}

impl_into_aggregate_tuple!(arity = 2, types = [(A, 0), (B, 1),]);

impl_into_aggregate_tuple!(arity = 3, types = [(A, 0), (B, 1), (C, 2),]);

impl_into_aggregate_tuple!(arity = 4, types = [(A, 0), (B, 1), (C, 2), (D, 3),]);

// ── Pending annotated queryset + terminal ────────────────────────────

/// Pending annotated query — produced by [`QuerySet::annotate`] and
/// terminated with [`Self::fetch_all`].
///
/// Holds the upstream queryset (for `FROM` + `WHERE` + `ORDER BY` +
/// `LIMIT` + `OFFSET`) plus the typed aggregate tuple that shapes the
/// SELECT list extensions.
///
/// Users typically receive `Vec<(T, A::Decoded)>` back — the
/// aggregate side of each row is shaped to match the closure's
/// return type.
#[must_use = "annotated queries are lazy — dropping one silently omits the query"]
pub struct AnnotatedQuerySet<T: Model, A: IntoAggregateTuple> {
    pub(crate) qs: QuerySet<T>,
    pub(crate) aggregates: A,
    pub(crate) qualify: Option<crate::expr::QualifyCondition>,
    pub(crate) _a: PhantomData<fn() -> A>,
}

impl<T: Model, A: IntoAggregateTuple> AnnotatedQuerySet<T, A> {
    /// Filter rows by an annotated window-function output.
    ///
    /// PostgreSQL 18 has no `QUALIFY` clause, so the predicate lowers to
    /// an outer `WHERE` over a derived table that wraps this annotated
    /// select. Reading the SQL: `SELECT * FROM (<inner annotated select>)
    /// AS __djogi_q WHERE <alias> <op> $N`.
    ///
    /// The closure receives `&A` — the same annotation value the user
    /// passed to [`QuerySet::annotate`]. Calling `.lt(...)` / `.lte(...)`
    /// / `.eq(...)` / `.gte(...)` / `.gt(...)` on a window function
    /// produces a [`QualifyCondition`](crate::QualifyCondition) bound to
    /// that function's `.alias("…")`, so there is no string lookup and
    /// no way to reference an alias that was not registered.
    ///
    /// ```ignore
    /// use djogi::prelude::*;
    ///
    /// let rows: Vec<(Elephant, i64)> = Elephant::objects()
    ///     .annotate(|e| RowNumber::new()
    ///         .partition_by(e.herd_id())
    ///         .order_by(e.score().desc())
    ///         .alias("rank"))
    ///     .qualify(|w| w.lte(3))
    ///     .fetch_all(&mut ctx)
    ///     .await?;
    /// ```
    #[must_use = "annotated queries are lazy — dropping one silently omits the query"]
    pub fn qualify<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&A) -> crate::expr::QualifyCondition,
    {
        let cond = f(&self.aggregates);
        self.qualify = Some(cond);
        self
    }
}

impl<T: Model, A: PlainAnnotationTuple + Send> AnnotatedQuerySet<T, A>
where
    T: FromPgRow + Send + Unpin,
{
    /// Execute the annotated query and collect every matching row into
    /// a `Vec<(T, A::Decoded)>`.
    ///
    /// Dispatches through the context's execution helpers — annotated
    /// queries work inside an `atomic()` scope. For tenant-keyed models,
    /// the terminal propagates the caller's auth tenant into the RLS GUC
    /// after local validation and before SQL emission, matching the
    /// ordinary `QuerySet` terminal contract.
    pub fn fetch_all<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Vec<(T, A::Decoded)>, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
        A: 'ctx,
        A::Decoded: Send + 'ctx,
    {
        async move {
            let AnnotatedQuerySet {
                qs,
                aggregates,
                qualify,
                ..
            } = self;
            // Short-circuit: `QuerySet::none()` yields an empty result
            // with no SQL round trip — same contract as `fetch_all`.
            if qs.is_empty() {
                return Ok(Vec::new());
            }

            // Validate DISTINCT modifier combinations before building SQL —
            // rejected combos surface as DjogiError::UnsupportedAggregate.
            aggregates.check_legality()?;

            // Reject pair-only aggregates on the single-Model annotate
            // path. Two signals together cover the rejection set:
            //
            //   - `requires_closure_pair_join()` (today:
            //     `PairClosureKinshipSum<C>`) flags slots that reference
            //     `la.` / `ra.` closure-pair aliases.
            //   - `requires_pair_tuple_scope()` (today:
            //     `PairAreaOverlapRatio<L, R>` plus every slot above —
            //     the closure-pair signal implies pair-tuple scope) flags
            //     slots that reference `l.` / `r.` pair-side aliases
            //     without necessarily needing closure metadata.
            //
            // Without the broader scope gate, `PairAreaOverlapRatio`
            // would compile into a single-Model `QuerySet::annotate(...)`
            // chain and surface as `42P01 missing FROM-clause entry for
            // table "l"` at execute time. The check turns that into a
            // typed `DjogiError::Validation` with a remediation hint —
            // same shape the joined path uses for its dual error of
            // "kinship aggregate without closure-pair join".
            if aggregates.requires_pair_tuple_scope() || aggregates.requires_closure_pair_join() {
                return Err(DjogiError::Validation(
                    "single-Model QuerySet::annotate cannot host a pair-tuple aggregate \
                     (e.g. PairClosureKinshipSum, PairAreaOverlapRatio). These aggregates \
                     reference the pair-tuple emitter's `l.` / `r.` / `la.` / `ra.` aliases \
                     which are only in scope inside a JoinedQuerySet. Use \
                     `model_objects.self_pairs().annotate(...)` (or \
                     `.left_join_closure_pair::<C>().annotate(...)` for closure-pair aggregates) \
                     to reach the joined-annotated terminal."
                        .to_string(),
                ));
            }

            crate::query::terminal::auto_set_tenant::<T>(ctx).await?;

            let acc = build_annotated_select_for_fetch(
                &qs,
                |acc| {
                    aggregates.push_plain_columns(acc);
                },
                qualify.as_ref(),
            )
            .map_err(DjogiError::from)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let rows = ctx.query_all(&sql, &params).await?;

            // Name-based decode: `T::from_pg_row` reads only the columns
            // `T` knows about (skipping the `__djogi_agg_N` aliases);
            // `A::decode_tuple` reads each aggregate slot by its
            // well-known alias. No offset math needed.
            let mut out: Vec<(T, A::Decoded)> = Vec::with_capacity(rows.len());
            for row in &rows {
                let model = T::from_pg_row(row)?;
                let agg = aggregates.decode_tuple(row).map_err(DjogiError::from)?;
                out.push((model, agg));
            }
            Ok(out)
        }
    }
}

// ── Entry point on QuerySet ──────────────────────────────────────────

impl<T: Model> QuerySet<T> {
    /// Augment this queryset with one or more aggregate columns.
    ///
    /// The closure receives a default-constructed `T::Fields` handle
    /// and returns either a single plain-annotation slot (arity 1) or
    /// a tuple of slots (arity 2..=4). For aggregate expressions, the
    /// plain ungrouped path accepts value aggregates only because it
    /// synthesizes `OVER ()`; ordered-set, hypothetical-set, and metadata
    /// aggregate kinds remain available through `QuerySet::aggregate` and
    /// grouped annotate where no synthesized window is added. The pending
    /// [`AnnotatedQuerySet`] is terminated with
    /// [`AnnotatedQuerySet::fetch_all`], which returns
    /// `Vec<(T, Decoded)>` where `Decoded` is the scalar (arity 1)
    /// or a Rust tuple (arity 2..=4).
    ///
    /// Per Phase 4 plan Q5, wider annotations (arity 3+) are
    /// discouraged in guide docs — shape into a local struct
    /// post-hoc for readability. There is no lint; wider annotations
    /// compile and run, they just scale poorly as the tuple widens.
    ///
    /// ```ignore
    /// use djogi::prelude::*;
    ///
    /// // arity-1 annotation: Vec<(Account, i64)>
    /// let rows: Vec<(Account, i64)> = Account::objects()
    ///     .annotate(|f| f.balance().sum())
    ///     .fetch_all(&mut ctx).await?;
    ///
    /// // arity-2 annotation: Vec<(Account, (i64, f64))>
    /// let rows: Vec<(Account, (i64, f64))> = Account::objects()
    ///     .annotate(|f| (f.balance().sum(), f.balance().avg()))
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    #[must_use = "annotated queries are lazy — dropping one silently omits the query"]
    pub fn annotate<F, A>(self, f: F) -> AnnotatedQuerySet<T, A>
    where
        F: FnOnce(T::Fields) -> A,
        A: PlainAnnotationTuple,
    {
        let aggregates = f(T::Fields::default());
        AnnotatedQuerySet {
            qs: self,
            aggregates,
            qualify: None,
            _a: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Emitter-level tests — assert on the shape of the SQL the
    //! annotate path emits for arity 1 and arity 2 tuples.

    use super::*;
    use crate::descriptor::ModelDescriptor;
    use crate::expr::{DenseRank, Expr, Rank, RowNumber};
    use crate::query::field::FieldRef;
    use crate::query::sql::build_select_with_annotations;

    struct Acc;
    impl crate::model::__sealed::Sealed for Acc {}
    #[allow(clippy::manual_async_fn)]
    impl Model for Acc {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "accs"
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

    // T3: SQL-text unit tests exercise `build_select_with_annotations`
    // which now bounds on `FromPgRow` so it can enumerate the canonical
    // column list instead of `t.*`. The stub claims a single column
    // `id` — enough to check the emitter's `t.<col>` shape without
    // pretending the fake model has a full schema.
    impl FromPgRow for Acc {
        const COLUMNS: &'static [&'static str] = &["id"];
        const COLUMN_LIST: &'static str = "id";
        fn from_pg_row(_row: &tokio_postgres::Row) -> Result<Self, crate::DjogiError> {
            unreachable!("SQL-text unit tests do not exercise row decode")
        }
    }

    #[test]
    fn annotate_arity_one_emits_expected_sql() {
        let qs: QuerySet<Acc> = QuerySet::new();
        let f: FieldRef<Acc, i64> = FieldRef::new("balance");
        let agg = f.sum();
        let acc = build_select_with_annotations(&qs, |acc| {
            agg.push_columns(acc);
        })
        .expect("annotate select");
        let sql = acc.sql();
        assert!(
            sql.contains(
                "SELECT t.id, (SUM(balance) OVER ())::BIGINT AS __djogi_agg_0 FROM accs AS t"
            ),
            "got: {sql}"
        );
    }

    #[test]
    fn annotate_arity_two_emits_both_aggregates() {
        let qs: QuerySet<Acc> = QuerySet::new();
        let f1: FieldRef<Acc, i64> = FieldRef::new("balance");
        let f2: FieldRef<Acc, i64> = FieldRef::new("balance");
        let tuple = (f1.sum(), f2.count());
        let acc = build_select_with_annotations(&qs, |acc| {
            tuple.push_columns(acc);
        })
        .expect("annotate select");
        let sql = acc.sql();
        assert!(
            sql.contains("(SUM(balance) OVER ())::BIGINT AS __djogi_agg_0"),
            "got: {sql}"
        );
        assert!(
            sql.contains("COUNT(balance) OVER () AS __djogi_agg_1"),
            "got: {sql}"
        );
    }

    #[test]
    fn aggregate_scalar_emits_select_agg_from_table() {
        // Scalar aggregate — the sibling `aggregate` terminal's SQL
        // shape. Pinned here so the test suite catches shape drift
        // without a live DB round trip.
        use crate::query::sql::build_aggregate_select;
        let qs: QuerySet<Acc> = QuerySet::new();
        let f: FieldRef<Acc, i64> = FieldRef::new("balance");
        let agg = f.sum();
        let acc = build_aggregate_select(&qs, &agg.node).expect("aggregate select");
        let sql = acc.sql();
        assert_eq!(
            sql.trim(),
            "SELECT (SUM(balance))::BIGINT FROM accs",
            "got: {sql}"
        );
    }

    #[test]
    fn aggregate_count_with_filter_emits_filter_clause() {
        use crate::query::sql::build_aggregate_select;
        let qs: QuerySet<Acc> = QuerySet::new();
        let f_count: FieldRef<Acc, i64> = FieldRef::new("balance");
        let f_cond: FieldRef<Acc, i64> = FieldRef::new("balance");
        let agg = f_count
            .count()
            .filter(f_cond.as_expr().lt(Expr::literal(0i64)));
        let acc = build_aggregate_select(&qs, &agg.node).expect("aggregate select");
        let sql = acc.sql();
        assert!(
            sql.contains("COUNT(balance) FILTER (WHERE balance < $1)"),
            "got: {sql}"
        );
    }

    #[test]
    fn row_number_window_annotation_emits_required_over_and_alias() {
        let qs: QuerySet<Acc> = QuerySet::new();
        let herd: FieldRef<Acc, i64> = FieldRef::new("herd_id");
        let score: FieldRef<Acc, i64> = FieldRef::new("score");
        let row_number = RowNumber::new()
            .partition_by(herd)
            .order_by(score.desc())
            .alias("rank");

        let acc = build_select_with_annotations(&qs, |acc| {
            row_number.push_columns(acc);
        })
        .expect("annotate select");
        let sql = acc.sql();

        assert!(
            sql.contains("ROW_NUMBER() OVER (PARTITION BY herd_id ORDER BY score DESC) AS rank"),
            "got: {sql}"
        );
    }

    #[test]
    fn rank_window_annotation_emits_required_over_and_alias() {
        let qs: QuerySet<Acc> = QuerySet::new();
        let herd: FieldRef<Acc, i64> = FieldRef::new("herd_id");
        let score: FieldRef<Acc, i64> = FieldRef::new("score");
        let rank = Rank::new()
            .partition_by(herd)
            .order_by(score.desc())
            .alias("rank");

        let acc = build_select_with_annotations(&qs, |acc| {
            rank.push_columns(acc);
        })
        .expect("annotate select");
        let sql = acc.sql();

        assert!(
            sql.contains("RANK() OVER (PARTITION BY herd_id ORDER BY score DESC) AS rank"),
            "got: {sql}"
        );
    }

    #[test]
    fn dense_rank_window_annotation_emits_required_over_and_alias() {
        let qs: QuerySet<Acc> = QuerySet::new();
        let herd: FieldRef<Acc, i64> = FieldRef::new("herd_id");
        let score: FieldRef<Acc, i64> = FieldRef::new("score");
        let dense_rank = DenseRank::new()
            .partition_by(herd)
            .order_by(score.desc())
            .alias("dense_rank");

        let acc = build_select_with_annotations(&qs, |acc| {
            dense_rank.push_columns(acc);
        })
        .expect("annotate select");
        let sql = acc.sql();

        assert!(
            sql.contains(
                "DENSE_RANK() OVER (PARTITION BY herd_id ORDER BY score DESC) AS dense_rank"
            ),
            "got: {sql}"
        );
    }

    #[test]
    fn qualify_lowers_to_derived_table_where() {
        let score: FieldRef<Acc, i64> = FieldRef::new("score");
        let annotated = QuerySet::<Acc>::new()
            .annotate(|_| RowNumber::new().order_by(score.desc()).alias("rank"))
            .qualify(|w| w.lte(3));

        let acc = build_annotated_select_for_fetch(
            &annotated.qs,
            |acc| annotated.aggregates.push_columns(acc),
            annotated.qualify.as_ref(),
        )
        .expect("annotated select");
        let sql = acc.sql();

        assert!(
            sql.starts_with(
                "SELECT * FROM (SELECT t.id, ROW_NUMBER() OVER (ORDER BY score DESC) AS rank FROM accs AS t"
            ),
            "got: {sql}"
        );
        assert!(
            sql.contains(") AS __djogi_q WHERE rank <= $1"),
            "got: {sql}"
        );
    }

    #[test]
    fn qualify_lowering_never_emits_qualify_clause_token() {
        let score: FieldRef<Acc, i64> = FieldRef::new("score");
        let annotated = QuerySet::<Acc>::new()
            .annotate(|_| RowNumber::new().order_by(score.desc()).alias("rank"))
            .qualify(|w| w.lte(3));

        let acc = build_annotated_select_for_fetch(
            &annotated.qs,
            |acc| annotated.aggregates.push_columns(acc),
            annotated.qualify.as_ref(),
        )
        .expect("annotated select");
        let sql = acc.sql();

        assert!(!sql.contains("QUALIFY"), "got: {sql}");
        assert!(!sql.contains("qualify"), "got: {sql}");
    }

    #[test]
    #[should_panic(expected = "is reserved")]
    fn alias_rejects_framework_reserved_djogi_q_prefix() {
        // `__djogi_q` is the derived-table alias the qualify emitter wraps
        // the inner select with. Allowing a user alias of `__djogi_q` would
        // produce ambiguous outer SQL.
        let _ = RowNumber::new().alias("__djogi_q");
    }

    #[test]
    #[should_panic(expected = "is reserved")]
    fn alias_rejects_framework_reserved_agg_slot_prefix() {
        // `__djogi_agg_N` is the aggregate-tuple slot alias used by row
        // decode. A user alias matching that namespace would silently
        // route window output to the wrong decode slot.
        let _ = Rank::new().alias("__djogi_agg_0");
    }

    // ── Cluster E T18-T19 — new window-only functions ────────────────────────

    #[test]
    fn percent_rank_window_emits_over_clause_and_alias() {
        use crate::expr::PercentRankWindow;
        let qs: QuerySet<Acc> = QuerySet::new();
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let pr = PercentRankWindow::new()
            .order_by(amount.desc())
            .alias("amount_pct");
        let acc = build_select_with_annotations(&qs, |acc| pr.push_columns(acc))
            .expect("annotate select");
        let sql = acc.sql();
        assert!(
            sql.contains("PERCENT_RANK() OVER (ORDER BY amount DESC) AS amount_pct"),
            "got: {sql}"
        );
    }

    #[test]
    fn cume_dist_window_emits_over_clause_and_alias() {
        use crate::expr::CumeDistWindow;
        let qs: QuerySet<Acc> = QuerySet::new();
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let cd = CumeDistWindow::new()
            .order_by(amount.asc())
            .alias("cume_dist");
        let acc = build_select_with_annotations(&qs, |acc| cd.push_columns(acc))
            .expect("annotate select");
        let sql = acc.sql();
        assert!(
            sql.contains("CUME_DIST() OVER (ORDER BY amount ASC) AS cume_dist"),
            "got: {sql}"
        );
    }

    #[test]
    fn ntile_window_binds_bucket_count_and_emits_over() {
        use crate::expr::NtileWindow;
        let qs: QuerySet<Acc> = QuerySet::new();
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let ntile = NtileWindow::new(4)
            .order_by(amount.desc())
            .alias("quartile");
        let acc = build_select_with_annotations(&qs, |acc| ntile.push_columns(acc))
            .expect("annotate select");
        let sql = acc.sql();
        assert!(
            sql.contains("NTILE($") && sql.contains(") OVER (ORDER BY amount DESC) AS quartile"),
            "got: {sql}"
        );
    }

    #[test]
    fn first_value_window_emits_column_and_over() {
        use crate::expr::FirstValueWindow;
        let qs: QuerySet<Acc> = QuerySet::new();
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let herd: FieldRef<Acc, i64> = FieldRef::new("herd_id");
        let fv: FirstValueWindow<i64> = FirstValueWindow::new(amount)
            .partition_by(herd)
            .order_by(amount.desc())
            .alias("top_amount");
        let acc = build_select_with_annotations(&qs, |acc| fv.push_columns(acc))
            .expect("annotate select");
        let sql = acc.sql();
        assert!(
            sql.contains(
                "FIRST_VALUE(amount) OVER (PARTITION BY herd_id ORDER BY amount DESC) AS top_amount"
            ),
            "got: {sql}"
        );
    }

    #[test]
    fn last_value_window_emits_column_and_over() {
        use crate::expr::LastValueWindow;
        let qs: QuerySet<Acc> = QuerySet::new();
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let lv: LastValueWindow<i64> = LastValueWindow::new(amount)
            .order_by(amount.asc())
            .alias("bottom_amount");
        let acc = build_select_with_annotations(&qs, |acc| lv.push_columns(acc))
            .expect("annotate select");
        let sql = acc.sql();
        assert!(
            sql.contains("LAST_VALUE(amount) OVER (ORDER BY amount ASC) AS bottom_amount"),
            "got: {sql}"
        );
    }

    #[test]
    fn lead_window_default_offset_emits_no_offset_arg() {
        use crate::expr::LeadWindow;
        let qs: QuerySet<Acc> = QuerySet::new();
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let lead: LeadWindow<i64> = LeadWindow::new(amount)
            .order_by(amount.asc())
            .alias("next_amount");
        let acc = build_select_with_annotations(&qs, |acc| lead.push_columns(acc))
            .expect("annotate select");
        let sql = acc.sql();
        assert!(
            sql.contains("LEAD(amount) OVER (ORDER BY amount ASC) AS next_amount"),
            "got: {sql}"
        );
    }

    #[test]
    fn lead_window_with_offset_binds_offset_arg() {
        use crate::expr::LeadWindow;
        let qs: QuerySet<Acc> = QuerySet::new();
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let lead: LeadWindow<i64> = LeadWindow::new(amount)
            .offset(3)
            .order_by(amount.asc())
            .alias("third_next_amount");
        let acc = build_select_with_annotations(&qs, |acc| lead.push_columns(acc))
            .expect("annotate select");
        let sql = acc.sql();
        assert!(
            sql.contains("LEAD(amount, $")
                && sql.contains(") OVER (ORDER BY amount ASC) AS third_next_amount"),
            "got: {sql}"
        );
    }

    #[test]
    fn lag_window_emits_lag_keyword() {
        use crate::expr::LagWindow;
        let qs: QuerySet<Acc> = QuerySet::new();
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let lag: LagWindow<i64> = LagWindow::new(amount)
            .order_by(amount.asc())
            .alias("prev_amount");
        let acc = build_select_with_annotations(&qs, |acc| lag.push_columns(acc))
            .expect("annotate select");
        let sql = acc.sql();
        assert!(
            sql.contains("LAG(amount) OVER (ORDER BY amount ASC) AS prev_amount"),
            "got: {sql}"
        );
    }

    #[test]
    fn nth_value_window_emits_column_and_n() {
        use crate::expr::NthValueWindow;
        let qs: QuerySet<Acc> = QuerySet::new();
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let nv: NthValueWindow<i64> = NthValueWindow::new(amount, 3)
            .order_by(amount.desc())
            .alias("third");
        let acc = build_select_with_annotations(&qs, |acc| nv.push_columns(acc))
            .expect("annotate select");
        let sql = acc.sql();
        assert!(
            sql.contains("NTH_VALUE(amount, $")
                && sql.contains(") OVER (ORDER BY amount DESC) AS third"),
            "got: {sql}"
        );
    }

    // ── T18-T19 coverage backfill (quality reviewer round-1 finding) ─────────
    //
    // First batch of T18-T19 tests covered bare emission. Quality
    // reviewer flagged that missing-`.alias()` rejection,
    // `partition_by` integration, and decode-type pinning weren't
    // covered. These tests close those gaps.

    #[test]
    fn lead_window_partition_by_renders_partition_clause() {
        use crate::expr::LeadWindow;
        let qs: QuerySet<Acc> = QuerySet::new();
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let session: FieldRef<Acc, i64> = FieldRef::new("session_id");
        let lead: LeadWindow<i64> = LeadWindow::new(amount)
            .partition_by(session)
            .order_by(amount.asc())
            .alias("next_amount");
        let acc = build_select_with_annotations(&qs, |acc| lead.push_columns(acc))
            .expect("annotate select");
        let sql = acc.sql();
        assert!(
            sql.contains(
                "LEAD(amount) OVER (PARTITION BY session_id ORDER BY amount ASC) AS next_amount"
            ),
            "PARTITION BY must render before ORDER BY in window clause, got: {sql}"
        );
    }

    #[test]
    fn ntile_window_partition_by_renders_partition_clause() {
        use crate::expr::NtileWindow;
        let qs: QuerySet<Acc> = QuerySet::new();
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let dept: FieldRef<Acc, i64> = FieldRef::new("dept_id");
        let ntile = NtileWindow::new(4)
            .partition_by(dept)
            .order_by(amount.desc())
            .alias("dept_quartile");
        let acc = build_select_with_annotations(&qs, |acc| ntile.push_columns(acc))
            .expect("annotate select");
        let sql = acc.sql();
        assert!(
            sql.contains("NTILE($")
                && sql.contains(
                    ") OVER (PARTITION BY dept_id ORDER BY amount DESC) AS dept_quartile"
                ),
            "got: {sql}"
        );
    }

    #[test]
    fn lead_window_decode_type_pinned_to_v() {
        // Compile-time pin: the phantom V on LeadWindow<V> drives the
        // AnnotationSlot::Decoded type. Constructing with a typed
        // FieldRef<Acc, V> yields LeadWindow<V> — verified by the
        // type ascriptions below.
        use crate::expr::LeadWindow;
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let _: LeadWindow<i64> = LeadWindow::new(amount);

        let label: FieldRef<Acc, String> = FieldRef::new("label");
        let _: LeadWindow<String> = LeadWindow::new(label);
    }

    #[test]
    fn first_value_window_decode_type_pinned_to_v() {
        use crate::expr::FirstValueWindow;
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let _: FirstValueWindow<i64> = FirstValueWindow::new(amount);

        let label: FieldRef<Acc, String> = FieldRef::new("label");
        let _: FirstValueWindow<String> = FirstValueWindow::new(label);
    }

    // GAP-4 closure (Codex T22 round-2): type-pin tests for the
    // remaining column-arg window-fn family. LastValueWindow,
    // LagWindow, and NthValueWindow are macro-generated from the
    // same template as FirstValue / Lead, but the type-pin
    // verification is per-type so a regression on any one would
    // surface independently.

    #[test]
    fn last_value_window_decode_type_pinned_to_v() {
        use crate::expr::LastValueWindow;
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let _: LastValueWindow<i64> = LastValueWindow::new(amount);

        let label: FieldRef<Acc, String> = FieldRef::new("label");
        let _: LastValueWindow<String> = LastValueWindow::new(label);
    }

    #[test]
    fn lag_window_decode_type_pinned_to_v() {
        use crate::expr::LagWindow;
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let _: LagWindow<i64> = LagWindow::new(amount);

        let label: FieldRef<Acc, String> = FieldRef::new("label");
        let _: LagWindow<String> = LagWindow::new(label);
    }

    #[test]
    fn nth_value_window_decode_type_pinned_to_v() {
        use crate::expr::NthValueWindow;
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let _: NthValueWindow<i64> = NthValueWindow::new(amount, 3);

        let label: FieldRef<Acc, String> = FieldRef::new("label");
        let _: NthValueWindow<String> = NthValueWindow::new(label, 5);
    }

    #[test]
    #[should_panic(expected = "is reserved")]
    fn lead_window_alias_rejects_djogi_prefix() {
        // Same `__djogi_*` reservation discipline as the rank-family
        // alias setter.
        use crate::expr::LeadWindow;
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let _: LeadWindow<i64> = LeadWindow::new(amount).alias("__djogi_q");
    }

    #[test]
    #[should_panic(expected = "is reserved")]
    fn ntile_window_alias_rejects_djogi_prefix() {
        use crate::expr::NtileWindow;
        let _ = NtileWindow::new(4).alias("__djogi_agg_0");
    }

    #[test]
    #[should_panic(expected = "is reserved")]
    fn percent_rank_window_alias_rejects_djogi_prefix() {
        use crate::expr::PercentRankWindow;
        let _ = PercentRankWindow::new().alias("__djogi_q");
    }

    // ── requires_pair_tuple_scope — default + tuple-OR semantics ──────
    //
    // The trait method [`AnnotationSlot::requires_pair_tuple_scope`]
    // defaults to `false`. Tuple impls OR across slots so a single
    // pair-only slot poisons the tuple for the single-Model / grouped
    // paths. The end-to-end live-DB rejection on
    // `AnnotatedQuerySet::fetch_all` and the grouped terminals is
    // covered by the integration-test surface; these unit tests cover
    // the signal-propagation invariants.

    /// Ordinary `AggregateExpr` slots (`f.col().sum()` etc.) MUST
    /// report `requires_pair_tuple_scope() = false`. These slots emit
    /// bare-column SQL like `SUM(balance)` that runs fine in a single-
    /// Model FROM clause; they are unrelated to the pair-tuple scope
    /// signal and must not be rejected by the single-Model annotate
    /// gate.
    #[test]
    fn aggregate_expr_default_requires_pair_tuple_scope_false() {
        let f: FieldRef<Acc, i64> = FieldRef::new("balance");
        let sum = f.sum();
        assert!(
            !AnnotationSlot::requires_pair_tuple_scope(&sum),
            "AggregateExpr<V> must default to requires_pair_tuple_scope() = false"
        );
        // Forwards through the IntoAggregateTuple blanket too.
        let tuple_view: &dyn IntoAggregateTuple<Decoded = i64> = &sum;
        assert!(
            !tuple_view.requires_pair_tuple_scope(),
            "IntoAggregateTuple blanket must forward AnnotationSlot::requires_pair_tuple_scope through"
        );
    }

    /// Window functions (rank, dense_rank, row_number, …) also default
    /// to `requires_pair_tuple_scope() = false`. Even pair-qualified
    /// window specs (`partition_by_pair(...)`) get their own
    /// `is_joined_safe()` opt-in surface; the pair-tuple-scope signal
    /// is for slots whose SQL **literally** contains `l.` / `r.` /
    /// `la.` / `ra.` aliases at emit time. Window-function output
    /// references the alias name (e.g. `RANK() AS my_rank`), so a
    /// `RowNumber::new().alias("rank")` is technically usable on a
    /// single-Model annotate.
    #[test]
    fn row_number_default_requires_pair_tuple_scope_false() {
        let rn = RowNumber::new().alias("rank");
        assert!(
            !AnnotationSlot::requires_pair_tuple_scope(&rn),
            "RowNumber must default to requires_pair_tuple_scope() = false"
        );
    }

    /// `()` (the no-aggregation case) MUST report
    /// `requires_pair_tuple_scope() = false`. Reverse direction of
    /// `is_joined_safe()` default (which is `true` for `()`) — the
    /// "OR-across-slots" semantics on the empty set yield `false`.
    #[test]
    fn unit_tuple_requires_pair_tuple_scope_false() {
        let unit: () = ();
        assert!(
            !IntoAggregateTuple::requires_pair_tuple_scope(&unit),
            "() / no-annotation case must report requires_pair_tuple_scope() = false"
        );
    }

    /// Tuple impls OR across slots: a 2-tuple where one slot reports
    /// `requires_pair_tuple_scope() = true` must poison the whole
    /// tuple. The custom test slot below mimics the override pattern
    /// `PairClosureKinshipSum` / `PairAreaOverlapRatio` use.
    #[test]
    fn tuple_with_pair_only_slot_propagates_through_or() {
        // Fabricate a minimal AnnotationSlot impl that overrides
        // `requires_pair_tuple_scope()` to true. Used only here to
        // exercise the tuple-OR plumbing without coupling annotate.rs's
        // unit tests to the spatial / closure-pair concrete slot types
        // (both of which live in joined.rs and pull additional context).
        struct PairOnlySlot;
        impl annotation_slot_sealed::Sealed for PairOnlySlot {}
        impl AnnotationSlot for PairOnlySlot {
            type Decoded = i64;
            fn push_column(&self, _acc: &mut SqlAccumulator, _slot: usize) {
                unreachable!("test slot — emitter never invoked")
            }
            fn push_column_bare(&self, _acc: &mut SqlAccumulator, _slot: usize) {
                unreachable!("test slot — emitter never invoked")
            }
            fn push_column_bare_after(
                &self,
                _acc: &mut SqlAccumulator,
                _slot: usize,
                _has_previous_columns: bool,
            ) {
                unreachable!("test slot — emitter never invoked")
            }
            fn decode_column(
                &self,
                _row: &tokio_postgres::Row,
                _slot: usize,
            ) -> Result<i64, tokio_postgres::Error> {
                unreachable!("test slot — decoder never invoked")
            }
            fn is_joined_safe(&self) -> bool {
                true
            }
            fn requires_pair_tuple_scope(&self) -> bool {
                true
            }
        }

        // Standalone — propagates through the single-slot blanket.
        let pair_only = PairOnlySlot;
        let tuple_view: &dyn IntoAggregateTuple<Decoded = i64> = &pair_only;
        assert!(
            tuple_view.requires_pair_tuple_scope(),
            "single-slot blanket must forward requires_pair_tuple_scope() = true"
        );

        // Arity-2: ordinary + pair-only must OR to true.
        let f: FieldRef<Acc, i64> = FieldRef::new("balance");
        let pair = (f.sum(), PairOnlySlot);
        assert!(
            pair.requires_pair_tuple_scope(),
            "arity-2 tuple with any pair-only slot must OR to true"
        );

        // Arity-2: ordinary + ordinary must remain false.
        let g: FieldRef<Acc, i64> = FieldRef::new("balance");
        let ordinary = (f.sum(), g.count());
        assert!(
            !ordinary.requires_pair_tuple_scope(),
            "arity-2 tuple of ordinary slots must remain false"
        );
    }

    // ── Issue #95 — f64 qualify for PercentRankWindow / CumeDistWindow ───────
    //
    // These tests pin the SQL lowering for f64 qualify thresholds on the two
    // FLOAT8-returning zero-arg window functions. Each test covers:
    //   - The correct derived-table shape (`SELECT * FROM (...) AS __djogi_q WHERE …`).
    //   - The correct operator (`<`, `<=`, `>=`, `>`, `=`).
    //   - A single bind slot allocated (the f64 threshold).
    //   - No `QUALIFY` token in the emitted SQL (Postgres 18 has no QUALIFY clause).
    //
    // The tests do NOT execute against a live database — they assert on the SQL
    // text emitted by `build_annotated_select_for_fetch`. Live-DB coverage is
    // provided by the existing window bench tests once the framework types are
    // exercised through `.annotate(...).qualify(...)`.

    #[test]
    fn percent_rank_qualify_lt_lowers_to_derived_table_where_f64() {
        use crate::expr::PercentRankWindow;
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let annotated = QuerySet::<Acc>::new()
            .annotate(|_| {
                PercentRankWindow::new()
                    .order_by(amount.desc())
                    .alias("amount_pct")
            })
            .qualify(|w| w.lt(0.5));

        let acc = build_annotated_select_for_fetch(
            &annotated.qs,
            |acc| annotated.aggregates.push_columns(acc),
            annotated.qualify.as_ref(),
        )
        .expect("annotated select for percent_rank qualify lt");
        let sql = acc.sql();

        // Inner select emits PERCENT_RANK with OVER clause and alias.
        assert!(
            sql.contains("PERCENT_RANK() OVER (ORDER BY amount DESC) AS amount_pct"),
            "inner PERCENT_RANK emission missing, got: {sql}"
        );
        // Outer WHERE references the alias with the correct operator and a bind slot.
        assert!(
            sql.contains(") AS __djogi_q WHERE amount_pct < $"),
            "outer WHERE predicate missing or malformed, got: {sql}"
        );
        // No literal QUALIFY token — Postgres 18 does not support it.
        assert!(
            !sql.contains("QUALIFY"),
            "QUALIFY token must not appear, got: {sql}"
        );
        // Exactly one bind slot: the f64 threshold.
        let (_, binds) = acc.into_parts();
        assert_eq!(
            binds.len(),
            1,
            "expected exactly one bind slot (the f64 threshold)"
        );
    }

    #[test]
    fn percent_rank_qualify_gte_lowers_to_derived_table_where_f64() {
        use crate::expr::PercentRankWindow;
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let annotated = QuerySet::<Acc>::new()
            .annotate(|_| {
                PercentRankWindow::new()
                    .order_by(amount.desc())
                    .alias("top_pct")
            })
            .qualify(|w| w.gte(0.9));

        let acc = build_annotated_select_for_fetch(
            &annotated.qs,
            |acc| annotated.aggregates.push_columns(acc),
            annotated.qualify.as_ref(),
        )
        .expect("annotated select for percent_rank qualify gte");
        let sql = acc.sql();

        assert!(
            sql.contains(") AS __djogi_q WHERE top_pct >= $"),
            "outer WHERE must use >= operator, got: {sql}"
        );
        let (_, binds) = acc.into_parts();
        assert_eq!(binds.len(), 1, "expected exactly one f64 bind slot");
    }

    #[test]
    fn cume_dist_qualify_lte_lowers_to_derived_table_where_f64() {
        use crate::expr::CumeDistWindow;
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let annotated = QuerySet::<Acc>::new()
            .annotate(|_| {
                CumeDistWindow::new()
                    .order_by(amount.asc())
                    .alias("cume_dist")
            })
            .qualify(|w| w.lte(0.25));

        let acc = build_annotated_select_for_fetch(
            &annotated.qs,
            |acc| annotated.aggregates.push_columns(acc),
            annotated.qualify.as_ref(),
        )
        .expect("annotated select for cume_dist qualify lte");
        let sql = acc.sql();

        assert!(
            sql.contains("CUME_DIST() OVER (ORDER BY amount ASC) AS cume_dist"),
            "inner CUME_DIST emission missing, got: {sql}"
        );
        assert!(
            sql.contains(") AS __djogi_q WHERE cume_dist <= $"),
            "outer WHERE must use <= operator, got: {sql}"
        );
        assert!(
            !sql.contains("QUALIFY"),
            "QUALIFY token must not appear, got: {sql}"
        );
        let (_, binds) = acc.into_parts();
        assert_eq!(
            binds.len(),
            1,
            "expected exactly one bind slot (the f64 threshold)"
        );
    }

    #[test]
    fn cume_dist_qualify_gt_lowers_to_derived_table_where_f64() {
        use crate::expr::CumeDistWindow;
        let amount: FieldRef<Acc, i64> = FieldRef::new("amount");
        let annotated = QuerySet::<Acc>::new()
            .annotate(|_| {
                CumeDistWindow::new()
                    .order_by(amount.asc())
                    .alias("cume_dist")
            })
            .qualify(|w| w.gt(0.0));

        let acc = build_annotated_select_for_fetch(
            &annotated.qs,
            |acc| annotated.aggregates.push_columns(acc),
            annotated.qualify.as_ref(),
        )
        .expect("annotated select for cume_dist qualify gt");
        let sql = acc.sql();

        assert!(
            sql.contains(") AS __djogi_q WHERE cume_dist > $"),
            "outer WHERE must use > operator, got: {sql}"
        );
        let (_, binds) = acc.into_parts();
        assert_eq!(binds.len(), 1, "expected exactly one f64 bind slot");
    }

    #[test]
    fn percent_rank_qualify_eq_lowers_to_derived_table_where_f64() {
        // Equality on FLOAT8 is valid SQL for exact boundary values (e.g.
        // the first row's PERCENT_RANK is exactly 0.0). This test pins the
        // SQL shape; adopters should prefer lt/lte/gte/gt for thresholds.
        use crate::expr::PercentRankWindow;
        let score: FieldRef<Acc, i64> = FieldRef::new("score");
        let annotated = QuerySet::<Acc>::new()
            .annotate(|_| PercentRankWindow::new().order_by(score.desc()).alias("pct"))
            .qualify(|w| w.eq(0.0));

        let acc = build_annotated_select_for_fetch(
            &annotated.qs,
            |acc| annotated.aggregates.push_columns(acc),
            annotated.qualify.as_ref(),
        )
        .expect("annotated select for percent_rank qualify eq");
        let sql = acc.sql();

        assert!(
            sql.contains(") AS __djogi_q WHERE pct = $"),
            "outer WHERE must use = operator, got: {sql}"
        );
        let (_, binds) = acc.into_parts();
        assert_eq!(binds.len(), 1, "expected exactly one f64 bind slot");
    }

    #[test]
    #[should_panic(expected = "qualify can only reference a window annotation")]
    fn percent_rank_qualify_without_alias_panics() {
        // Calling a qualify helper before `.alias("…")` must panic with the
        // standard "alias not set" diagnostic — same contract as the rank family.
        use crate::expr::PercentRankWindow;
        let _ = PercentRankWindow::new().lt(0.5);
    }

    #[test]
    #[should_panic(expected = "qualify can only reference a window annotation")]
    fn cume_dist_qualify_without_alias_panics() {
        use crate::expr::CumeDistWindow;
        let _ = CumeDistWindow::new().gte(0.9);
    }

    #[test]
    fn percent_rank_qualify_bind_count_is_one_f64() {
        // Narrowly pins that exactly ONE bind slot is consumed by the qualify
        // predicate. The f64 threshold must not be emitted inline into the SQL
        // text (SQL injection safety) and must not consume two slots.
        use crate::expr::PercentRankWindow;
        let cond = PercentRankWindow::new().alias("pct").lt(0.75);
        let mut acc = crate::pg::accumulator::SqlAccumulator::new("");
        cond.push_outer_where(&mut acc);
        assert_eq!(
            acc.bind_count(),
            1,
            "qualify predicate must consume exactly one bind slot"
        );
        assert!(
            acc.sql().contains("$1"),
            "bind placeholder must appear in SQL, got: {}",
            acc.sql()
        );
        assert!(
            !acc.sql().contains("0.75"),
            "threshold value must NOT appear verbatim in SQL text (injection safety), got: {}",
            acc.sql()
        );
    }
}
