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
//! - `AggregateExpr<V>` (arity 1) — `Decoded = V`
//! - `(AggregateExpr<V1>, AggregateExpr<V2>)` — `Decoded = (V1, V2)`
//! - `(AggregateExpr<V1>, AggregateExpr<V2>, AggregateExpr<V3>)`
//!   — `Decoded = (V1, V2, V3)`
//! - `(AggregateExpr<V1>, AggregateExpr<V2>, AggregateExpr<V3>,
//!   AggregateExpr<V4>)` — `Decoded = (V1, V2, V3, V4)`
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
use crate::expr::{AggregateExpr, DenseRank, Rank, RowNumber};
use crate::model::Model;
use crate::pg::accumulator::{SqlAccumulator, as_params};
use crate::pg::decode::FromPgRow;
use crate::query::queryset::QuerySet;
use crate::query::sql::{build_annotated_select_for_fetch, emit_aggregate_with_window_and_cast};
use std::future::Future;
use std::marker::PhantomData;

// ── Sealed seal ──────────────────────────────────────────────────────

mod sealed {
    //! Crate-private seal. Downstream code cannot name
    //! `sealed::Sealed`, so the only `IntoAggregateTuple` impls are
    //! the framework-blessed ones below.
    pub trait Sealed {}
}

mod annotation_slot_sealed {
    pub trait Sealed {}
}

const AGG_ALIASES: [&str; 4] = [
    "__djogi_agg_0",
    "__djogi_agg_1",
    "__djogi_agg_2",
    "__djogi_agg_3",
];

fn aggregate_alias(slot: usize) -> &'static str {
    AGG_ALIASES[slot]
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
}

// ── Single annotation slots ──────────────────────────────────────────

// Each aggregate is emitted with `OVER ()` so the annotate SELECT-list
// stays valid without a `GROUP BY` clause. For self-column aggregates
// (the Task 4 shape) `OVER ()` produces the table-wide aggregate value
// on every row — matches the typical Django intuition of `annotate`
// producing "one scalar per returned row". Reverse-relation aggregates
// (`f.orders.count()`) will ship in Task 5 and may need a different
// emission strategy (LATERAL joins + per-parent partitions); the
// window-function form here is deliberately the simplest that works
// for the Task 4 scope without requiring GROUP BY semantics.

impl<V> annotation_slot_sealed::Sealed for AggregateExpr<V> {}
impl<V> AnnotationSlot for AggregateExpr<V>
where
    V: for<'a> postgres_types::FromSql<'a> + Send + Unpin + 'static,
{
    type Decoded = V;

    fn push_column(&self, acc: &mut SqlAccumulator, slot: usize) {
        acc.push_sql(", ");
        emit_aggregate_with_window_and_cast(acc, &self.node);
        acc.push_sql(" AS ");
        acc.push_sql(aggregate_alias(slot));
    }

    fn push_column_bare(&self, acc: &mut SqlAccumulator, slot: usize) {
        acc.push_sql(", ");
        crate::query::sql::emit_aggregate_with_cast(acc, &self.node);
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

macro_rules! impl_window_annotation_slot {
    ($type_name:ty, $display:literal) => {
        impl annotation_slot_sealed::Sealed for $type_name {}

        impl AnnotationSlot for $type_name {
            type Decoded = i64;

            fn push_column(&self, acc: &mut SqlAccumulator, _slot: usize) {
                acc.push_sql(", ");
                self.push_annotated_column(acc);
            }

            fn push_column_bare(&self, acc: &mut SqlAccumulator, slot: usize) {
                self.push_column(acc, slot);
            }

            fn decode_column(
                &self,
                row: &tokio_postgres::Row,
                _slot: usize,
            ) -> Result<Self::Decoded, tokio_postgres::Error> {
                row.try_get::<_, i64>(
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
        }
    };
}

impl_window_annotation_slot!(RowNumber, "RowNumber");
impl_window_annotation_slot!(Rank, "Rank");
impl_window_annotation_slot!(DenseRank, "DenseRank");

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
        self.push_column_bare(acc, 0);
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
}

impl sealed::Sealed for () {}

impl IntoAggregateTuple for () {
    type Decoded = ();

    fn push_columns(&self, _acc: &mut SqlAccumulator) {}

    fn push_columns_bare(&self, _acc: &mut SqlAccumulator) {}

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
                $(
                    self.$slot.push_column_bare(acc, $slot);
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

impl<T: Model, A: IntoAggregateTuple + Send> AnnotatedQuerySet<T, A>
where
    T: FromPgRow + Send + Unpin,
{
    /// Execute the annotated query and collect every matching row into
    /// a `Vec<(T, A::Decoded)>`.
    ///
    /// Dispatches through the context's execution helpers — annotated
    /// queries work inside an `atomic()` scope.
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

            let acc = build_annotated_select_for_fetch(
                &qs,
                |acc| {
                    aggregates.push_columns(acc);
                },
                qualify.as_ref(),
            );
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
    /// and returns either a single [`AggregateExpr<V>`] (arity 1) or
    /// a tuple of aggregates (arity 2..=4). The pending
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
        A: IntoAggregateTuple,
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
        });
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
        });
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
        let acc = build_aggregate_select(&qs, &agg.node);
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
        let acc = build_aggregate_select(&qs, &agg.node);
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
        });
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
        });
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
        });
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
        );
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
        );
        let sql = acc.sql();

        assert!(!sql.contains("QUALIFY"), "got: {sql}");
        assert!(!sql.contains("qualify"), "got: {sql}");
    }
}
