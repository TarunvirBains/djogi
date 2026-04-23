//! Grouped query state types. See Phase 6.5 plan for the type-state
//! contract and method legality table.
//!
//! # Type-state transitions
//!
//! - `QuerySet<T>` → `GroupedQuerySet<T, K>` via `.group_by`
//! - `GroupedQuerySet<T, K>` → `GroupedAnnotatedQuerySet<T, K, A>` via `.annotate`
//! - `GroupedAnnotatedQuerySet<T, K, A>` is the only state with terminals
//!   (`.fetch_all`, `.stream`)
//!
//! Premature `.fetch_all` on `GroupedQuerySet<T, K>` is a compile error
//! (no such method exists). This is enforced structurally rather than via
//! runtime checks.

#![allow(clippy::manual_async_fn)]

use crate::model::Model;
use crate::pg::accumulator::SqlAccumulator;
use crate::query::field::FieldRef;
use crate::query::queryset::QuerySet;
use std::marker::PhantomData;

mod sealed {
    pub trait Sealed {}
}

/// Typed group-key tuple — produced by the closure passed to
/// `QuerySet::group_by`. Arity 1..=4 supported; wider shapes steer
/// users to a local struct post-hoc (same policy as annotate).
pub trait IntoGroupKeyTuple: sealed::Sealed {
    /// The Rust tuple the terminal decodes each grouped row into.
    type Decoded;

    /// Emit the column list for the `GROUP BY` clause onto `acc`.
    fn push_group_by_columns(&self, acc: &mut SqlAccumulator);

    /// Emit the same column list as the SELECT-list leading columns.
    fn push_select_columns(&self, acc: &mut SqlAccumulator);

    /// Decode the N leading columns of a grouped row into `Decoded`.
    fn decode_tuple(row: &tokio_postgres::Row) -> Result<Self::Decoded, tokio_postgres::Error>;
}

// ── Arity 1: single FieldRef<M, V> ───────────────────────────────────────

impl<M: Model, V> sealed::Sealed for FieldRef<M, V> {}

impl<M: Model, V> IntoGroupKeyTuple for FieldRef<M, V>
where
    V: for<'a> postgres_types::FromSql<'a> + Send + Unpin + 'static,
{
    type Decoded = V;

    fn push_group_by_columns(&self, acc: &mut SqlAccumulator) {
        acc.push_sql(self.column());
    }

    fn push_select_columns(&self, acc: &mut SqlAccumulator) {
        acc.push_sql(self.column());
    }

    fn decode_tuple(row: &tokio_postgres::Row) -> Result<Self::Decoded, tokio_postgres::Error> {
        row.try_get::<_, V>(0)
    }
}

// ── Arity 2..=4: tuples of FieldRef<M, V_i> ──────────────────────────────

macro_rules! impl_into_group_key_tuple {
    (
        arity = $arity:tt,
        types = [ $( ($ty:ident, $slot:tt, $pos:literal) ),+ $(,)? ]
    ) => {
        impl<M: Model, $($ty),+> sealed::Sealed for ( $(FieldRef<M, $ty>,)+ ) {}

        impl<M: Model, $($ty),+> IntoGroupKeyTuple for ( $(FieldRef<M, $ty>,)+ )
        where
            $( $ty: for<'a> postgres_types::FromSql<'a> + Send + Unpin + 'static, )+
        {
            type Decoded = ( $($ty,)+ );

            fn push_group_by_columns(&self, acc: &mut SqlAccumulator) {
                let mut first = true;
                $(
                    if !first { acc.push_sql(", "); }
                    first = false;
                    acc.push_sql(self.$slot.column());
                )+
                let _ = first;
            }

            fn push_select_columns(&self, acc: &mut SqlAccumulator) {
                let mut first = true;
                $(
                    if !first { acc.push_sql(", "); }
                    first = false;
                    acc.push_sql(self.$slot.column());
                )+
                let _ = first;
            }

            fn decode_tuple(row: &tokio_postgres::Row) -> Result<Self::Decoded, tokio_postgres::Error> {
                Ok((
                    $( row.try_get::<_, $ty>($pos)?, )+
                ))
            }
        }
    };
}

impl_into_group_key_tuple!(arity = 2, types = [(A, 0, 0), (B, 1, 1)]);
impl_into_group_key_tuple!(arity = 3, types = [(A, 0, 0), (B, 1, 1), (C, 2, 2)]);
impl_into_group_key_tuple!(
    arity = 4,
    types = [(A, 0, 0), (B, 1, 1), (C, 2, 2), (D, 3, 3)]
);

// ── GroupedQuerySet ───────────────────────────────────────────────────────

/// Grouping mode for `GROUP BY` variant.
///
/// `Plain` emits a plain `GROUP BY (col, ...)`. `Rollup` and `Cube` are
/// supported; `GROUPING SETS` support lands in T2 (it requires a richer
/// multi-set-list payload that changes the variant shape — `#[non_exhaustive]`
/// lets that change land without a breaking API change).
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum GroupingMode {
    /// `GROUP BY col [, col ...]`
    Plain,
    /// `GROUP BY ROLLUP (col [, col ...])`
    Rollup,
    /// `GROUP BY CUBE (col [, col ...])`
    Cube,
}

/// Grouped queryset with no annotations yet. No terminal available —
/// user must call `.annotate(...)` before fetching.
///
/// This is the intermediate state produced by `QuerySet::group_by`. Dropping
/// one without annotating is flagged by the `#[must_use]` attribute — the
/// query is silently discarded if the result is not used.
#[must_use = "grouped queries are lazy — dropping one silently omits the query"]
pub struct GroupedQuerySet<T: Model, K: IntoGroupKeyTuple> {
    pub(crate) qs: QuerySet<T>,
    pub(crate) keys: K,
    pub(crate) grouping: GroupingMode,
    pub(crate) _k: PhantomData<fn() -> K>,
}

// ── GroupedAnnotatedQuerySet ──────────────────────────────────────────────

/// Grouped and annotated queryset — the only grouped state that has terminals.
///
/// Produced by `GroupedQuerySet::annotate`. Terminals (`fetch_all`) execute the
/// `SELECT keys, aggregates FROM table [WHERE ...] GROUP BY keys
/// [HAVING ...] [ORDER BY ...] [LIMIT ...] [OFFSET ...]` query and decode the
/// result into `Vec<(K::Decoded, A::Decoded)>`.
#[must_use = "grouped queries are lazy — dropping one silently omits the query"]
pub struct GroupedAnnotatedQuerySet<
    T: Model,
    K: IntoGroupKeyTuple,
    A: crate::query::annotate::IntoAggregateTuple,
> {
    pub(crate) qs: QuerySet<T>,
    pub(crate) keys: K,
    pub(crate) grouping: GroupingMode,
    pub(crate) aggregates: A,
    pub(crate) having: Option<crate::expr::node::ExprNode>,
    pub(crate) order: Vec<crate::query::order::OrderExpr>,
    pub(crate) limit: Option<u64>,
    pub(crate) offset: Option<u64>,
    pub(crate) _k: PhantomData<fn() -> K>,
    pub(crate) _a: PhantomData<fn() -> A>,
}

// ── GroupedQuerySet::annotate transition ─────────────────────────────────

impl<T: Model, K: IntoGroupKeyTuple> GroupedQuerySet<T, K> {
    /// Attach aggregate expressions to this grouped query, transitioning into
    /// `GroupedAnnotatedQuerySet<T, K, A>` — the state that has terminals.
    ///
    /// The closure receives a default-constructed `T::Fields` and returns one
    /// aggregate (arity 1) or a tuple (arity 2..=4). Until this is called,
    /// no terminal method is available; the type-state enforces correct
    /// call order at compile time.
    #[must_use = "grouped queries are lazy — dropping one silently omits the query"]
    pub fn annotate<F, A>(self, f: F) -> GroupedAnnotatedQuerySet<T, K, A>
    where
        F: FnOnce(T::Fields) -> A,
        A: crate::query::annotate::IntoAggregateTuple,
    {
        let aggregates = f(T::Fields::default());
        GroupedAnnotatedQuerySet {
            qs: self.qs,
            keys: self.keys,
            grouping: self.grouping,
            aggregates,
            having: None,
            order: Vec::new(),
            limit: None,
            offset: None,
            _k: PhantomData,
            _a: PhantomData,
        }
    }
}

// ── GroupedAnnotatedQuerySet methods ─────────────────────────────────────

// `.having` and `.order_by` consume clones of `K` and `A` so the closure
// receives them by value — the caller can call consuming methods like
// `AggregateExpr::gt(100)` directly without extra `.clone()` noise.
// `Clone` bounds are intentional here and limited to these two methods.
impl<T: Model, K: IntoGroupKeyTuple + Clone, A: crate::query::annotate::IntoAggregateTuple + Clone>
    GroupedAnnotatedQuerySet<T, K, A>
{
    /// Attach a `HAVING` clause to the grouped query.
    ///
    /// The closure receives clones of the key tuple and aggregate tuple so the
    /// caller can call consuming methods directly (e.g. `a.gt(100)`). Calling
    /// `.having(...)` twice replaces the previous clause — last call wins,
    /// matching `QuerySet::limit`.
    #[must_use = "grouped queries are lazy — dropping one silently omits the query"]
    pub fn having<F>(mut self, f: F) -> Self
    where
        F: FnOnce(K, A) -> crate::expr::Expr<bool>,
    {
        let cond = f(self.keys.clone(), self.aggregates.clone());
        self.having = Some(cond.node);
        self
    }

    /// Append an `ORDER BY` expression to the grouped query.
    ///
    /// The closure receives clones of the key tuple and aggregate tuple.
    /// Multiple calls append; they do not replace (same append semantics as
    /// `QuerySet::order_by`). The `ORDER BY` clause is emitted after `HAVING`.
    #[must_use = "grouped queries are lazy — dropping one silently omits the query"]
    pub fn order_by<F>(mut self, f: F) -> Self
    where
        F: FnOnce(K, A) -> crate::query::order::OrderExpr,
    {
        let order = f(self.keys.clone(), self.aggregates.clone());
        self.order.push(order);
        self
    }
}

// `.limit` and `.offset` need no `Clone` bound — they don't touch `K` or `A`.
impl<T: Model, K: IntoGroupKeyTuple, A: crate::query::annotate::IntoAggregateTuple>
    GroupedAnnotatedQuerySet<T, K, A>
{
    /// Set the `LIMIT` for the grouped query.
    pub fn limit(mut self, n: u64) -> Self {
        self.limit = Some(n);
        self
    }

    /// Set the `OFFSET` for the grouped query.
    pub fn offset(mut self, n: u64) -> Self {
        self.offset = Some(n);
        self
    }
}

// ── GroupedAnnotatedQuerySet::fetch_all terminal ─────────────────────────

impl<T: Model, K: IntoGroupKeyTuple + Send, A: crate::query::annotate::IntoAggregateTuple + Send>
    GroupedAnnotatedQuerySet<T, K, A>
where
    T: Send,
    K::Decoded: Send,
    A::Decoded: Send,
{
    /// Execute the grouped query and collect every result row into
    /// `Vec<(K::Decoded, A::Decoded)>`.
    ///
    /// Keys are decoded positionally (ordinals 0..N_keys). Aggregates are
    /// decoded by alias (`__djogi_agg_N`). Live round-trip coverage is in T14.
    #[allow(clippy::type_complexity)]
    pub fn fetch_all<'ctx>(
        self,
        ctx: &'ctx mut crate::context::DjogiContext,
    ) -> impl std::future::Future<Output = Result<Vec<(K::Decoded, A::Decoded)>, crate::DjogiError>>
    + Send
    + 'ctx
    where
        T: 'ctx,
        K: 'ctx,
        A: 'ctx,
        K::Decoded: 'ctx,
        A::Decoded: 'ctx,
    {
        async move {
            let acc = crate::query::sql::build_grouped_annotated_select(&self);
            let (sql, binds) = acc.into_parts();
            let params: Vec<&(dyn postgres_types::ToSql + Sync)> = binds
                .iter()
                .map(|b| b.as_ref() as &(dyn postgres_types::ToSql + Sync))
                .collect();
            let rows = ctx.query_all(&sql, &params).await?;
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                let k = K::decode_tuple(row).map_err(crate::DjogiError::from)?;
                let a = A::decode_tuple(row).map_err(crate::DjogiError::from)?;
                out.push((k, a));
            }
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::ModelDescriptor;

    // Inert local model — mirrors the stub used across annotate.rs tests.
    // The full `impl Model for Fake` is required because the grouped types
    // carry `T: Model` bounds.
    struct Fake;
    impl crate::model::__sealed::Sealed for Fake {}
    #[allow(clippy::manual_async_fn)]
    impl Model for Fake {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "fakes"
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

    // Step 1.2 — arity-1
    #[test]
    fn arity_one_field_ref_implements_into_group_key_tuple() {
        fn assert_bound<T: IntoGroupKeyTuple>() {}
        assert_bound::<FieldRef<Fake, i64>>();
    }

    // Step 1.3 — arity 2..=4
    #[test]
    fn arity_two_tuple_implements_into_group_key_tuple() {
        fn assert_bound<T: IntoGroupKeyTuple>() {}
        assert_bound::<(FieldRef<Fake, i64>, FieldRef<Fake, String>)>();
    }

    #[test]
    fn arity_three_tuple_implements_into_group_key_tuple() {
        fn assert_bound<T: IntoGroupKeyTuple>() {}
        assert_bound::<(
            FieldRef<Fake, i64>,
            FieldRef<Fake, String>,
            FieldRef<Fake, i32>,
        )>();
    }

    #[test]
    fn arity_four_tuple_implements_into_group_key_tuple() {
        fn assert_bound<T: IntoGroupKeyTuple>() {}
        assert_bound::<(
            FieldRef<Fake, i64>,
            FieldRef<Fake, String>,
            FieldRef<Fake, i32>,
            FieldRef<Fake, bool>,
        )>();
    }

    // Step 1.4 — QuerySet::group_by type transition
    #[test]
    fn queryset_group_by_returns_grouped_queryset() {
        let qs: QuerySet<Fake> = QuerySet::new();
        let f: FieldRef<Fake, i64> = FieldRef::new("org_id");
        let _grouped: GroupedQuerySet<Fake, FieldRef<Fake, i64>> = qs.group_by(|_| f);
    }

    // Step 1.6 — .annotate transition
    #[test]
    fn group_by_then_annotate_returns_grouped_annotated_queryset() {
        use crate::expr::AggregateExpr;
        let qs: QuerySet<Fake> = QuerySet::new();
        let keys: FieldRef<Fake, i64> = FieldRef::new("org_id");
        let vals: FieldRef<Fake, i64> = FieldRef::new("amount");
        let _gaq: GroupedAnnotatedQuerySet<Fake, FieldRef<Fake, i64>, AggregateExpr<i64>> =
            qs.group_by(|_| keys).annotate(|_| vals.sum());
    }

    // Step 1.9 — HAVING, ORDER BY, LIMIT, OFFSET SQL emission
    #[test]
    fn having_clause_emits_after_group_by() {
        use crate::query::sql::build_grouped_annotated_select;
        let qs: QuerySet<Fake> = QuerySet::new();
        let keys: FieldRef<Fake, i64> = FieldRef::new("org_id");
        let vals: FieldRef<Fake, i64> = FieldRef::new("amount");
        let gaq = qs
            .group_by(|_| keys)
            .annotate(|_| vals.sum())
            .having(|k, _a| k.as_expr().gt(crate::expr::Expr::literal(0i64)));
        let acc = build_grouped_annotated_select(&gaq);
        let sql = acc.sql();
        assert!(
            sql.contains("GROUP BY") && sql.contains("HAVING"),
            "got: {sql}"
        );
        // HAVING must come after GROUP BY
        let group_pos = sql.find("GROUP BY").unwrap();
        let having_pos = sql.find("HAVING").unwrap();
        assert!(
            having_pos > group_pos,
            "HAVING must appear after GROUP BY, got: {sql}"
        );
    }

    #[test]
    fn grouped_order_by_emits_after_group_by() {
        use crate::query::sql::build_grouped_annotated_select;
        let qs: QuerySet<Fake> = QuerySet::new();
        let keys: FieldRef<Fake, i64> = FieldRef::new("org_id");
        let vals: FieldRef<Fake, i64> = FieldRef::new("amount");
        let gaq = qs
            .group_by(|_| keys)
            .annotate(|_| vals.sum())
            .order_by(|k, _a| k.asc());
        let acc = build_grouped_annotated_select(&gaq);
        let sql = acc.sql();
        assert!(
            sql.contains("GROUP BY") && sql.contains("ORDER BY"),
            "got: {sql}"
        );
        let group_pos = sql.find("GROUP BY").unwrap();
        let order_pos = sql.find("ORDER BY").unwrap();
        assert!(
            order_pos > group_pos,
            "ORDER BY must appear after GROUP BY, got: {sql}"
        );
    }

    #[test]
    fn grouped_limit_offset_emit_in_order() {
        use crate::query::sql::build_grouped_annotated_select;
        let qs: QuerySet<Fake> = QuerySet::new();
        let keys: FieldRef<Fake, i64> = FieldRef::new("org_id");
        let vals: FieldRef<Fake, i64> = FieldRef::new("amount");
        let gaq = qs
            .group_by(|_| keys)
            .annotate(|_| vals.sum())
            .limit(10)
            .offset(20);
        let acc = build_grouped_annotated_select(&gaq);
        let sql = acc.sql();
        assert!(sql.contains("LIMIT"), "expected LIMIT, got: {sql}");
        assert!(sql.contains("OFFSET"), "expected OFFSET, got: {sql}");
        let limit_pos = sql.find("LIMIT").unwrap();
        let offset_pos = sql.find("OFFSET").unwrap();
        assert!(
            offset_pos > limit_pos,
            "OFFSET must come after LIMIT, got: {sql}"
        );
    }

    // P1-2 — arity-2 key tuple: verify Clone is satisfied transitively and
    // that `.having` emits a HAVING clause when K is a 2-tuple.
    #[test]
    fn having_on_arity_two_key_emits_having_clause() {
        use crate::query::sql::build_grouped_annotated_select;
        let qs: QuerySet<Fake> = QuerySet::new();
        let k1: FieldRef<Fake, i64> = FieldRef::new("org_id");
        let k2: FieldRef<Fake, i64> = FieldRef::new("region_id");
        let vals: FieldRef<Fake, i64> = FieldRef::new("amount");
        // `.having` closure receives (K, A) by value — both must be Clone.
        // FieldRef<M, V> is Copy, so 2-tuples of FieldRef are also Clone.
        let gaq = qs
            .group_by(|_| (k1, k2))
            .annotate(|_| vals.sum())
            .having(|(_k1, _k2), _a| {
                crate::expr::Expr::literal(1i64).gt(crate::expr::Expr::literal(0i64))
            });
        let acc = build_grouped_annotated_select(&gaq);
        let sql = acc.sql();
        assert!(
            sql.contains("GROUP BY") && sql.contains("HAVING"),
            "expected GROUP BY + HAVING for arity-2 key, got: {sql}"
        );
    }

    // T2 — .rollup and .cube entry points produce GroupedQuerySet with the
    // correct GroupingMode. The mode is verified via the SQL emitter — calling
    // .annotate then build_grouped_annotated_select and asserting the clause.

    #[test]
    fn queryset_rollup_returns_grouped_queryset_with_rollup_mode() {
        use crate::query::sql::build_grouped_annotated_select;
        let qs: QuerySet<Fake> = QuerySet::new();
        let f: FieldRef<Fake, i64> = FieldRef::new("org_id");
        let vals: FieldRef<Fake, i64> = FieldRef::new("amount");
        let gaq = qs.rollup(|_| f).annotate(|_| vals.sum());
        let acc = build_grouped_annotated_select(&gaq);
        let sql = acc.sql();
        assert!(
            sql.contains("GROUP BY ROLLUP (org_id)"),
            "expected ROLLUP clause via .rollup entry point, got: {sql}"
        );
    }

    #[test]
    fn queryset_cube_returns_grouped_queryset_with_cube_mode() {
        use crate::query::sql::build_grouped_annotated_select;
        let qs: QuerySet<Fake> = QuerySet::new();
        let f: FieldRef<Fake, i64> = FieldRef::new("org_id");
        let vals: FieldRef<Fake, i64> = FieldRef::new("amount");
        let gaq = qs.cube(|_| f).annotate(|_| vals.sum());
        let acc = build_grouped_annotated_select(&gaq);
        let sql = acc.sql();
        assert!(
            sql.contains("GROUP BY CUBE (org_id)"),
            "expected CUBE clause via .cube entry point, got: {sql}"
        );
    }

    // P1-2 — arity-3 key tuple: same check.
    #[test]
    fn having_on_arity_three_key_emits_having_clause() {
        use crate::query::sql::build_grouped_annotated_select;
        let qs: QuerySet<Fake> = QuerySet::new();
        let k1: FieldRef<Fake, i64> = FieldRef::new("org_id");
        let k2: FieldRef<Fake, i64> = FieldRef::new("region_id");
        let k3: FieldRef<Fake, i64> = FieldRef::new("product_id");
        let vals: FieldRef<Fake, i64> = FieldRef::new("amount");
        let gaq = qs
            .group_by(|_| (k1, k2, k3))
            .annotate(|_| vals.sum())
            .having(|(_k1, _k2, _k3), _a| {
                crate::expr::Expr::literal(1i64).gt(crate::expr::Expr::literal(0i64))
            });
        let acc = build_grouped_annotated_select(&gaq);
        let sql = acc.sql();
        assert!(
            sql.contains("GROUP BY") && sql.contains("HAVING"),
            "expected GROUP BY + HAVING for arity-3 key, got: {sql}"
        );
    }
}
