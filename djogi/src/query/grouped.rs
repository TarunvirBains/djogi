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

/// Crate-private seal for `IntoGroupKeyTuple`.
///
/// The `pub(crate)` visibility lets sibling modules (notably
/// `spatial_grouping.rs`) implement `IntoGroupKeyTuple` for their key types
/// without weakening the seal against downstream crates — the trait is still
/// un-namable outside `djogi`.
pub(crate) mod sealed {
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

// ── Spatial group source ─────────────────────────────────────────────────────
//
// `SpatialGroupSource` consolidates the three spatial emission strategies
// (region-join, DBSCAN window, geohash scalar) into a single enum so
// `GroupedQuerySet` / `GroupedAnnotatedQuerySet` carries one optional field
// and the SQL builder dispatches on a single match arm rather than
// cascading through three separate Option fields.

/// Discriminated union of the three spatial group-source strategies.
///
/// - `Join` — region path: LEFT JOIN + `ST_Contains` + GROUP BY region PK.
/// - `Cluster` — DBSCAN path: `ST_ClusterDBSCAN(...) OVER ()` window aggregate.
/// - `Geohash` — geohash path: `ST_GeoHash(..., precision)` scalar function.
///
/// Stored as `Option<SpatialGroupSource>` on `GroupedQuerySet` and
/// `GroupedAnnotatedQuerySet`; `None` means a plain non-spatial GROUP BY.
#[cfg(feature = "spatial")]
#[derive(Debug, Clone)]
pub(crate) enum SpatialGroupSource {
    /// Region-join path.
    Join(crate::query::spatial_grouping::SpatialJoinSpec),
    /// DBSCAN clustering path.
    Cluster(crate::query::spatial_grouping::ClusterSpec),
    /// Geohash-bucket path.
    Geohash(crate::query::spatial_grouping::GeohashSpec),
}

// `IntoGroupKeyTuple` impls for the spatial key types (`RegionKey<R>`,
// `ClusterId`, `GeohashKey`) live alongside their definitions in
// `spatial_grouping.rs`. The seal module above is `pub(crate)` so spatial
// impls can name `sealed::Sealed` directly without weakening the public seal.

// ── Arity 0: unit key () — used exclusively by group_by_sets ─────────────
//
// GROUPING SETS emits its own column list directly from the `Sets` payload;
// the key tuple plays no role in SQL emission for that mode. The `()` impl
// exists only so `GroupedQuerySet<T, ()>` satisfies the `IntoGroupKeyTuple`
// bound — it is a structural marker, not a real decoder.

impl sealed::Sealed for () {}

impl IntoGroupKeyTuple for () {
    type Decoded = ();

    fn push_group_by_columns(&self, _acc: &mut SqlAccumulator) {
        // No-op — GROUPING SETS emits its column list from the Sets payload,
        // not from the key tuple. This method is never called for Sets mode.
    }

    fn push_select_columns(&self, _acc: &mut SqlAccumulator) {
        // No-op — the SELECT list for a group_by_sets query consists only of
        // aggregate columns; there are no typed key columns to project.
    }

    fn decode_tuple(_row: &tokio_postgres::Row) -> Result<Self::Decoded, tokio_postgres::Error> {
        // The unit key decodes as `()` — callers access grouped-set column
        // values via raw row access on the returned `tokio_postgres::Row`,
        // not through this typed decode path.
        Ok(())
    }
}

// ── GroupedQuerySet ───────────────────────────────────────────────────────

/// Grouping mode for `GROUP BY` variant.
///
/// `Plain` emits a plain `GROUP BY col [, col ...]`. `Rollup` and `Cube`
/// wrap the column list in `ROLLUP (...)` and `CUBE (...)` respectively.
/// `Sets` emits `GROUPING SETS (...)` with an explicit per-set column list,
/// enabling arbitrary subtotal combinations in a single query pass.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum GroupingMode {
    /// `GROUP BY col [, col ...]`
    Plain,
    /// `GROUP BY ROLLUP (col [, col ...])`
    Rollup,
    /// `GROUP BY CUBE (col [, col ...])`
    Cube,
    /// `GROUP BY GROUPING SETS ((col_a), (col_b), ...)`. Each inner
    /// `Vec<&'static str>` is one grouping set's column list. Column names
    /// are `&'static str` because they come from `FieldRef::column()` —
    /// validated upstream by `assert_plain_ident`.
    Sets(Vec<Vec<&'static str>>),
}

/// Grouped queryset with no annotations yet. No terminal available —
/// user must call `.annotate(...)` before fetching.
///
/// This is the intermediate state produced by `QuerySet::group_by`. Dropping
/// one without annotating is flagged by the `#[must_use]` attribute — the
/// query is silently discarded if the result is not used.
///
/// The optional `spatial_source` field carries the spatial group-source spec
/// for spatially-derived group keys (`group_by_region`, `cluster_by_proximity`,
/// `bucket_by_cell`). `None` means a plain `GROUP BY col` query. The SQL
/// builder reads this field to decide which emission path to take.
#[must_use = "grouped queries are lazy — dropping one silently omits the query"]
pub struct GroupedQuerySet<T: Model, K: IntoGroupKeyTuple> {
    pub(crate) qs: QuerySet<T>,
    pub(crate) keys: K,
    pub(crate) grouping: GroupingMode,
    /// Spatial group source — `None` for all plain GROUP BY paths.
    #[cfg(feature = "spatial")]
    pub(crate) spatial_source: Option<SpatialGroupSource>,
    pub(crate) _k: PhantomData<fn() -> K>,
}

// ── GroupedAnnotatedQuerySet ──────────────────────────────────────────────

/// Grouped and annotated queryset — the only grouped state that has terminals.
///
/// Produced by `GroupedQuerySet::annotate`. Terminals (`fetch_all`) execute the
/// `SELECT keys, aggregates FROM table [WHERE ...] GROUP BY keys
/// [HAVING ...] [ORDER BY ...] [LIMIT ...] [OFFSET ...]` query and decode the
/// result into `Vec<(K::Decoded, A::Decoded)>`.
///
/// The optional `spatial_source` field carries the spatial group-source spec
/// propagated from `GroupedQuerySet`. `None` for all plain GROUP BY paths.
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
    /// Spatial group source propagated from `GroupedQuerySet`. `None` for
    /// plain GROUP BY queries; `Some` for any spatial grouping path.
    #[cfg(feature = "spatial")]
    pub(crate) spatial_source: Option<SpatialGroupSource>,
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
            // Propagate any spatial group source from the GroupedQuerySet so
            // the SQL builder can read it on the annotated state's fetch_all
            // path.
            #[cfg(feature = "spatial")]
            spatial_source: self.spatial_source,
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
            // Validate DISTINCT modifier combinations before building SQL —
            // rejected combos surface as DjogiError::UnsupportedAggregate.
            self.aggregates.check_legality()?;
            let acc = crate::query::sql::build_grouped_annotated_select(&self)
                .map_err(crate::DjogiError::from)?;
            // Defensive alias-collision check — fires only if a future API
            // extension introduces a path that collides with a group-key
            // column name or another __djogi_agg_N alias. Today's emitter
            // cannot produce a collision; the check guards future regressions.
            crate::query::sql::assert_no_alias_collision(acc.sql())?;
            let (sql, binds) = acc.into_parts();
            let params: Vec<&(dyn postgres_types::ToSql + Sync)> = binds
                .iter()
                .map(|b| b.as_ref() as &(dyn postgres_types::ToSql + Sync))
                .collect();
            let rows = ctx.query_all(&sql, &params).await?;
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                let k = K::decode_tuple(row).map_err(crate::DjogiError::from)?;
                let a = self
                    .aggregates
                    .decode_tuple(row)
                    .map_err(crate::DjogiError::from)?;
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
        let acc = build_grouped_annotated_select(&gaq).expect("grouped select");
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
        let acc = build_grouped_annotated_select(&gaq).expect("grouped select");
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
        let acc = build_grouped_annotated_select(&gaq).expect("grouped select");
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
        let acc = build_grouped_annotated_select(&gaq).expect("grouped select");
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
        let acc = build_grouped_annotated_select(&gaq).expect("grouped select");
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
        let acc = build_grouped_annotated_select(&gaq).expect("grouped select");
        let sql = acc.sql();
        assert!(
            sql.contains("GROUP BY CUBE (org_id)"),
            "expected CUBE clause via .cube entry point, got: {sql}"
        );
    }

    // T2 — .group_by_sets entry point produces GroupedQuerySet<T, ()>
    // and the emitter outputs GROUPING SETS (...).

    #[test]
    fn queryset_group_by_sets_returns_grouped_queryset_unit_key() {
        // Type-level check: group_by_sets returns GroupedQuerySet<T, ()>.
        // Also verifies the GROUPING SETS clause is emitted correctly via
        // the SQL emitter.
        use crate::query::sql::build_grouped_annotated_select;
        let qs: QuerySet<Fake> = QuerySet::new();
        let vals: FieldRef<Fake, i64> = FieldRef::new("amount");
        let gaq = qs
            .group_by_sets(|_| ["org_id", "region"])
            .annotate(|_| vals.sum());
        let acc = build_grouped_annotated_select(&gaq).expect("grouped select");
        let sql = acc.sql();
        assert!(
            sql.contains("GROUPING SETS ((org_id), (region))"),
            "expected GROUPING SETS clause, got: {sql}"
        );
    }

    // T11 — .grouping_sets() entry point supports multi-column sets per
    // group AND the empty grand-total set.

    #[test]
    fn queryset_grouping_sets_supports_multi_column_sets() {
        use crate::query::sql::build_grouped_annotated_select;
        let qs: QuerySet<Fake> = QuerySet::new();
        let vals: FieldRef<Fake, i64> = FieldRef::new("amount");
        let gaq = qs
            .grouping_sets(|_| vec![vec!["region", "dept"], vec!["region"], vec![]])
            .annotate(|_| vals.sum());
        let acc = build_grouped_annotated_select(&gaq).expect("grouped select");
        let sql = acc.sql();
        assert!(
            sql.contains("GROUPING SETS ((region, dept), (region), ())"),
            "expected multi-column + empty-set GROUPING SETS clause, got: {sql}"
        );
    }

    #[test]
    fn queryset_grouping_sets_extracts_columns_from_field_refs() {
        // Verify the closure receives `T::Fields` and adopters can call
        // `.column()` on each FieldRef to extract column names.
        use crate::query::sql::build_grouped_annotated_select;
        let qs: QuerySet<Fake> = QuerySet::new();
        let vals: FieldRef<Fake, i64> = FieldRef::new("amount");
        let region: FieldRef<Fake, i64> = FieldRef::new("region");
        let dept: FieldRef<Fake, i64> = FieldRef::new("dept");
        let gaq = qs
            .grouping_sets(|_| {
                vec![
                    vec![region.column(), dept.column()],
                    vec![region.column()],
                    vec![],
                ]
            })
            .annotate(|_| vals.sum());
        let acc = build_grouped_annotated_select(&gaq).expect("grouped select");
        let sql = acc.sql();
        assert!(
            sql.contains("GROUPING SETS ((region, dept), (region), ())"),
            "expected sets extracted via FieldRef::column(), got: {sql}"
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
        let acc = build_grouped_annotated_select(&gaq).expect("grouped select");
        let sql = acc.sql();
        assert!(
            sql.contains("GROUP BY") && sql.contains("HAVING"),
            "expected GROUP BY + HAVING for arity-3 key, got: {sql}"
        );
    }
}
