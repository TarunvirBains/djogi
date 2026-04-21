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
//! SELECT t.*, <agg_0> AS __djogi_agg_0, <agg_1> AS __djogi_agg_1, ...
//! FROM <table> AS t
//! [WHERE ...]
//! [ORDER BY ...]
//! [LIMIT $n] [OFFSET $n]
//! ```
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
//! # Why name-based decode for `T`
//!
//! The main row contains both `T`'s columns (from `t.*`) and the
//! aggregate columns (from `<agg> AS __djogi_agg_N`). `T: FromPgRowBridge`
//! looks up columns by name and ignores columns it doesn't know
//! about, so the aggregate aliases don't interfere with the `T`
//! decode. The aggregate tuple then reads each slot's value by
//! the `__djogi_agg_N` alias directly from the same `Row`.

#![allow(clippy::manual_async_fn)]

use crate::DjogiError;
use crate::context::DjogiContext;
use crate::expr::AggregateExpr;
use crate::model::Model;
use crate::pg::accumulator::SqlAccumulator;
use crate::pg::decode::FromPgRowBridge;
use crate::query::queryset::QuerySet;
use crate::query::sql::{build_select_with_annotations, emit_aggregate_with_window_and_cast};
use postgres_types::ToSql;
use std::future::Future;
use std::marker::PhantomData;

// ── Sealed seal ──────────────────────────────────────────────────────

mod sealed {
    //! Crate-private seal. Downstream code cannot name
    //! `sealed::Sealed`, so the only `IntoAggregateTuple` impls are
    //! the framework-blessed ones below.
    pub trait Sealed {}
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
/// `Decoded` is the tuple type users receive at fetch time: the
/// scalar for arity 1, a Rust tuple for arities 2..=4.
pub trait IntoAggregateTuple: sealed::Sealed {
    /// Rust tuple type returned inside `Vec<(T, Decoded)>`.
    type Decoded;

    /// Push the aggregate SELECT-list columns onto `acc`, each prefixed
    /// with `, ` and aliased as `__djogi_agg_{N}`.
    ///
    /// Indexing starts at 0; callers already pushed `SELECT t.*` so
    /// every push here begins with a comma.
    fn push_columns(&self, acc: &mut SqlAccumulator);

    /// Decode the aggregate columns from `row` into [`Self::Decoded`].
    ///
    /// The aliases are fixed (`__djogi_agg_0`, `__djogi_agg_1`, ...)
    /// so the impl reads each slot by name. Offset-based decoding is
    /// not used because `row.try_get` by name is more robust to
    /// column-ordering surprises (though the framework never reorders
    /// the SELECT list in practice).
    fn decode_tuple(row: &tokio_postgres::Row) -> Result<Self::Decoded, tokio_postgres::Error>;
}

// ── Arity 1: single AggregateExpr<V> ─────────────────────────────────

// Each aggregate is emitted with `OVER ()` so the annotate SELECT-list
// stays valid without a `GROUP BY` clause. For self-column aggregates
// (the Task 4 shape) `OVER ()` produces the table-wide aggregate value
// on every row — matches the typical Django intuition of `annotate`
// producing "one scalar per returned row". Reverse-relation aggregates
// (`f.orders.count()`) will ship in Task 5 and may need a different
// emission strategy (LATERAL joins + per-parent partitions); the
// window-function form here is deliberately the simplest that works
// for the Task 4 scope without requiring GROUP BY semantics.

impl<V> sealed::Sealed for AggregateExpr<V> {}
impl<V> IntoAggregateTuple for AggregateExpr<V>
where
    V: for<'a> postgres_types::FromSql<'a> + Send + Unpin + 'static,
{
    type Decoded = V;

    fn push_columns(&self, acc: &mut SqlAccumulator) {
        acc.push_sql(", ");
        emit_aggregate_with_window_and_cast(acc, &self.node);
        acc.push_sql(" AS __djogi_agg_0");
    }

    fn decode_tuple(row: &tokio_postgres::Row) -> Result<Self::Decoded, tokio_postgres::Error> {
        row.try_get::<_, V>("__djogi_agg_0")
    }
}

// ── Arity 2..=4: tuples of AggregateExpr<V_i> ────────────────────────
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
        types = [ $( ($ty:ident, $slot:tt, $alias:literal) ),+ $(,)? ]
    ) => {
        impl<$($ty),+> sealed::Sealed for ( $(AggregateExpr<$ty>,)+ ) {}

        impl<$($ty),+> IntoAggregateTuple for ( $(AggregateExpr<$ty>,)+ )
        where
            $(
                $ty: for<'a> postgres_types::FromSql<'a>
                    + Send
                    + Unpin
                    + 'static,
            )+
        {
            type Decoded = ( $($ty,)+ );

            fn push_columns(&self, acc: &mut SqlAccumulator) {
                $(
                    acc.push_sql(", ");
                    emit_aggregate_with_window_and_cast(acc, &self.$slot.node);
                    acc.push_sql(concat!(" AS ", $alias));
                )+
            }

            fn decode_tuple(row: &tokio_postgres::Row) -> Result<Self::Decoded, tokio_postgres::Error> {
                Ok((
                    $(
                        row.try_get::<_, $ty>($alias)?,
                    )+
                ))
            }
        }
    };
}

impl_into_aggregate_tuple!(
    arity = 2,
    types = [(A, 0, "__djogi_agg_0"), (B, 1, "__djogi_agg_1"),]
);

impl_into_aggregate_tuple!(
    arity = 3,
    types = [
        (A, 0, "__djogi_agg_0"),
        (B, 1, "__djogi_agg_1"),
        (C, 2, "__djogi_agg_2"),
    ]
);

impl_into_aggregate_tuple!(
    arity = 4,
    types = [
        (A, 0, "__djogi_agg_0"),
        (B, 1, "__djogi_agg_1"),
        (C, 2, "__djogi_agg_2"),
        (D, 3, "__djogi_agg_3"),
    ]
);

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
    pub(crate) _a: PhantomData<fn() -> A>,
}

impl<T: Model, A: IntoAggregateTuple + Send> AnnotatedQuerySet<T, A>
where
    T: FromPgRowBridge + Send + Unpin,
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
            let AnnotatedQuerySet { qs, aggregates, .. } = self;
            // Short-circuit: `QuerySet::none()` yields an empty result
            // with no SQL round trip — same contract as `fetch_all`.
            if qs.is_empty() {
                return Ok(Vec::new());
            }

            let acc = build_select_with_annotations(&qs, |acc| {
                aggregates.push_columns(acc);
            });
            let (sql, binds) = acc.into_parts();
            let params: Vec<&(dyn ToSql + Sync)> = binds
                .iter()
                .map(|b| b.as_ref() as &(dyn ToSql + Sync))
                .collect();
            let rows = ctx.query_all(&sql, &params).await?;

            // Name-based decode: `T::__from_pg_row` reads only the columns
            // `T` knows about (skipping the `__djogi_agg_N` aliases);
            // `A::decode_tuple` reads each aggregate slot by its
            // well-known alias. No offset math needed.
            let mut out: Vec<(T, A::Decoded)> = Vec::with_capacity(rows.len());
            for row in &rows {
                let model = T::__from_pg_row(row)?;
                let agg = A::decode_tuple(row).map_err(DjogiError::from)?;
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
    use crate::expr::Expr;
    use crate::query::field::FieldRef;

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
                "SELECT t.*, (SUM(balance) OVER ())::BIGINT AS __djogi_agg_0 FROM accs AS t"
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
}
