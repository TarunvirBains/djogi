use crate::Result;
use crate::context::DjogiContext;
use crate::error::DjogiError;
use crate::model::Model;
use crate::pg::accumulator::{SqlAccumulator, as_params};
use crate::pg::decode::{FromJoinedPgRow, FromPgRow};
use crate::query::joined::{
    LEFT_ALIAS, LEFT_COLUMN_PREFIX, PairSide, RIGHT_ALIAS, RIGHT_COLUMN_PREFIX,
    push_aliased_columns,
};
use crate::query::lock::LockMode;
use crate::query::portable::SqlEmitContext;
use crate::query::queryset::DistinctMode;
use crate::query::queryset::QuerySet;
use crate::query::sql::{build_select, push_tail_with_ctx};
use crate::query::terminal::auto_set_tenant;
use std::marker::PhantomData;

/// Mode marker for a `JOIN LATERAL` where the parent row is dropped if there is no child row.
pub struct InnerLateral;

/// Mode marker for a `LEFT JOIN LATERAL` where the parent row is preserved with `None` if there is no child row.
pub struct LeftLateral;

/// A query representing a lateral join between an outer queryset `L` and an inner queryset `R`.
pub struct LateralQuerySet<L: Model, R: Model, M = InnerLateral> {
    pub(crate) outer: QuerySet<L>,
    pub(crate) inner: QuerySet<R>,
    pub(crate) _mode: PhantomData<M>,
}

impl<L: Model + FromPgRow, R: Model + FromPgRow, M> LateralQuerySet<L, R, M> {
    fn validate(&self) -> Result<()> {
        if !self.outer.prefetch_paths.is_empty() || !self.inner.prefetch_paths.is_empty() {
            return Err(DjogiError::Validation(
                "Lateral queries do not support prefetch paths".into(),
            ));
        }
        if !self.outer.select_related_paths.is_empty()
            || !self.inner.select_related_paths.is_empty()
        {
            return Err(DjogiError::Validation(
                "Lateral queries do not support select_related".into(),
            ));
        }
        if self.outer.cache_target.is_some() || self.inner.cache_target.is_some() {
            return Err(DjogiError::Validation(
                "Lateral queries do not support cache_target".into(),
            ));
        }
        if !matches!(self.outer.lock, LockMode::None) || !matches!(self.inner.lock, LockMode::None)
        {
            return Err(DjogiError::Validation(
                "Lateral queries do not support row locks".into(),
            ));
        }
        Ok(())
    }

    fn build_inner_lateral_select(&self, is_left: bool) -> Result<SqlAccumulator> {
        // Inner lateral query
        let inner_for_sql;
        let inner = if self.inner.is_empty {
            // Structural-none inner query: preserve the lateral shape but
            // force the subquery to return no rows so LEFT JOIN LATERAL keeps
            // the outer row and decodes the right side as `None`.
            inner_for_sql = {
                let mut qs = self.inner.clone();
                qs.condition = crate::query::Q::always_false();
                qs
            };
            &inner_for_sql
        } else {
            &self.inner
        };

        let mut inner_acc = SqlAccumulator::new("");
        match &inner.distinct {
            DistinctMode::None => {
                inner_acc.push_sql("SELECT ");
            }
            DistinctMode::Plain => {
                inner_acc.push_sql("SELECT DISTINCT ");
            }
            DistinctMode::On(cols) => {
                inner_acc.push_sql("SELECT DISTINCT ON (");
                inner_acc.push_csv(cols.iter().copied());
                inner_acc.push_sql(") ");
            }
        }
        // We only need R columns, no alias prefixes for the subquery's own projection.
        for (i, col) in <R as FromPgRow>::COLUMNS.iter().enumerate() {
            if i > 0 {
                inner_acc.push_sql(", ");
            }
            inner_acc.push_sql(col);
        }
        if is_left {
            inner_acc.push_sql(", TRUE AS __djogi_lateral_present");
        }
        inner_acc.push_sql(" FROM ");
        inner_acc.push_sql(R::table_name());
        push_tail_with_ctx(
            &mut inner_acc,
            inner,
            SqlEmitContext::lateral_inner_scope::<L>(LEFT_ALIAS),
        )?;

        Ok(inner_acc)
    }

    pub(crate) fn build_sql(
        &self,
        is_left: bool,
        is_count: bool,
        final_tuple_limit: Option<i64>,
    ) -> Result<SqlAccumulator> {
        let mut acc = SqlAccumulator::new("");

        if is_count {
            acc.push_sql("SELECT COUNT(*) FROM (");
        }

        acc.push_sql("SELECT ");
        push_aliased_columns::<L>(&mut acc, PairSide::Left, true);
        push_aliased_columns::<R>(&mut acc, PairSide::Right, false);

        if is_left {
            acc.push_sql(", ");
            acc.push_sql(RIGHT_ALIAS);
            acc.push_sql(".__djogi_lateral_present");
        }

        // Outer queryset is treated as the source relation so its WHERE / DISTINCT /
        // ORDER / LIMIT / OFFSET semantics apply before lateral fan-out.
        acc.push_sql(" FROM (");
        let outer_acc = build_select(&self.outer)?;
        acc.extend_with(outer_acc);
        acc.push_sql(") AS ");
        acc.push_sql(LEFT_ALIAS);

        if is_left {
            acc.push_sql(" LEFT JOIN LATERAL (");
        } else {
            acc.push_sql(" JOIN LATERAL (");
        }

        let inner_acc = self.build_inner_lateral_select(is_left)?;
        acc.extend_with(inner_acc);

        acc.push_sql(") AS ");
        acc.push_sql(RIGHT_ALIAS);
        acc.push_sql(" ON TRUE");

        if let Some(limit) = final_tuple_limit {
            acc.push_sql(" LIMIT ");
            acc.push_bind(limit);
        }

        if is_count {
            acc.push_sql(")");
        }

        Ok(acc)
    }
}

impl<L: Model + FromJoinedPgRow + FromPgRow, R: Model + FromJoinedPgRow + FromPgRow>
    LateralQuerySet<L, R, InnerLateral>
{
    /// Build the lateral SELECT SQL this queryset would execute, without
    /// touching a database. **Internal-test plumbing — never call
    /// this from adopter code.**
    #[doc(hidden)]
    pub fn __sql_for_test(&self) -> Result<String> {
        let acc = self.build_sql(false, false, None)?;
        let (sql, _binds) = acc.into_parts();
        Ok(sql)
    }

    /// Build the lateral COUNT SQL this queryset would execute, without
    /// touching a database. **Internal-test plumbing — never call
    /// this from adopter code.**
    #[doc(hidden)]
    pub fn __count_sql_for_test(&self) -> Result<String> {
        let acc = self.build_sql(false, true, None)?;
        let (sql, _binds) = acc.into_parts();
        Ok(sql)
    }

    pub async fn count(self, ctx: &mut DjogiContext) -> Result<i64> {
        self.validate()?;
        if self.outer.is_empty || self.inner.is_empty {
            return Ok(0);
        }
        auto_set_tenant::<L>(ctx).await?;
        auto_set_tenant::<R>(ctx).await?;

        let acc = self.build_sql(false, true, None)?;
        let (sql, binds) = acc.into_parts();
        let params = as_params(&binds);
        let row = ctx.query_one(&sql, &params).await?;
        Ok(row.get(0))
    }

    pub async fn fetch_all(self, ctx: &mut DjogiContext) -> Result<Vec<(L, R)>> {
        self.validate()?;
        if self.outer.is_empty || self.inner.is_empty {
            return Ok(Vec::new());
        }
        auto_set_tenant::<L>(ctx).await?;
        auto_set_tenant::<R>(ctx).await?;

        let acc = self.build_sql(false, false, None)?;
        let (sql, binds) = acc.into_parts();
        let params = as_params(&binds);
        let rows = ctx.query_all(&sql, &params).await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push((
                L::from_joined_pg_row(&row, LEFT_COLUMN_PREFIX)?,
                R::from_joined_pg_row(&row, RIGHT_COLUMN_PREFIX)?,
            ));
        }
        Ok(out)
    }

    pub async fn first(self, ctx: &mut DjogiContext) -> Result<Option<(L, R)>> {
        self.validate()?;
        if self.outer.is_empty || self.inner.is_empty {
            return Ok(None);
        }
        auto_set_tenant::<L>(ctx).await?;
        auto_set_tenant::<R>(ctx).await?;

        let acc = self.build_sql(false, false, Some(1))?;
        let (sql, binds) = acc.into_parts();
        let params = as_params(&binds);
        let row = ctx.query_opt(&sql, &params).await?;
        let Some(row) = row else {
            return Ok(None);
        };

        Ok(Some((
            L::from_joined_pg_row(&row, LEFT_COLUMN_PREFIX)?,
            R::from_joined_pg_row(&row, RIGHT_COLUMN_PREFIX)?,
        )))
    }
}

impl<L: Model + FromJoinedPgRow + FromPgRow, R: Model + FromJoinedPgRow + FromPgRow>
    LateralQuerySet<L, R, LeftLateral>
{
    /// Build the lateral SELECT SQL this queryset would execute, without
    /// touching a database. **Internal-test plumbing — never call
    /// this from adopter code.**
    #[doc(hidden)]
    pub fn __sql_for_test(&self) -> Result<String> {
        let acc = self.build_sql(true, false, None)?;
        let (sql, _binds) = acc.into_parts();
        Ok(sql)
    }

    /// Build the lateral COUNT SQL this queryset would execute, without
    /// touching a database. **Internal-test plumbing — never call
    /// this from adopter code.**
    #[doc(hidden)]
    pub fn __count_sql_for_test(&self) -> Result<String> {
        let acc = self.build_sql(true, true, None)?;
        let (sql, _binds) = acc.into_parts();
        Ok(sql)
    }

    pub async fn count(self, ctx: &mut DjogiContext) -> Result<i64> {
        self.validate()?;
        if self.outer.is_empty {
            return Ok(0);
        }
        auto_set_tenant::<L>(ctx).await?;
        auto_set_tenant::<R>(ctx).await?;

        let acc = self.build_sql(true, true, None)?;
        let (sql, binds) = acc.into_parts();
        let params = as_params(&binds);
        let row = ctx.query_one(&sql, &params).await?;
        Ok(row.get(0))
    }

    pub async fn fetch_all(self, ctx: &mut DjogiContext) -> Result<Vec<(L, Option<R>)>> {
        self.validate()?;
        if self.outer.is_empty {
            return Ok(Vec::new());
        }
        auto_set_tenant::<L>(ctx).await?;
        auto_set_tenant::<R>(ctx).await?;

        let acc = self.build_sql(true, false, None)?;
        let (sql, binds) = acc.into_parts();
        let params = as_params(&binds);
        let rows = ctx.query_all(&sql, &params).await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let left = L::from_joined_pg_row(&row, LEFT_COLUMN_PREFIX)?;
            let present: Option<bool> = row.get("__djogi_lateral_present");
            let right = if present.unwrap_or(false) {
                Some(R::from_joined_pg_row(&row, RIGHT_COLUMN_PREFIX)?)
            } else {
                None
            };
            out.push((left, right));
        }
        Ok(out)
    }

    pub async fn first(self, ctx: &mut DjogiContext) -> Result<Option<(L, Option<R>)>> {
        self.validate()?;
        if self.outer.is_empty {
            return Ok(None);
        }
        auto_set_tenant::<L>(ctx).await?;
        auto_set_tenant::<R>(ctx).await?;

        let acc = self.build_sql(true, false, Some(1))?;
        let (sql, binds) = acc.into_parts();
        let params = as_params(&binds);
        let row = ctx.query_opt(&sql, &params).await?;
        let Some(row) = row else {
            return Ok(None);
        };

        let left = L::from_joined_pg_row(&row, LEFT_COLUMN_PREFIX)?;
        let present: Option<bool> = row.get("__djogi_lateral_present");
        let right = if present.unwrap_or(false) {
            Some(R::from_joined_pg_row(&row, RIGHT_COLUMN_PREFIX)?)
        } else {
            None
        };

        Ok(Some((left, right)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::ModelDescriptor;
    use crate::pg::decode::{FromJoinedPgRow, FromPgRow};
    use crate::query::field::{DjogiField, djogi_field_macro_support::__make_djogi_field};
    use std::future::Future;

    #[derive(Copy, Clone, Default)]
    struct BrokenFields;

    struct BrokenModel {
        score: i32,
    }

    impl BrokenFields {
        fn score(self) -> DjogiField<BrokenModel, i32> {
            __make_djogi_field::<BrokenModel, i32>("score", |m| &m.score)
        }
    }

    impl crate::model::__sealed::Sealed for BrokenModel {}

    // Hand-written test fixtures mirror `Model`'s explicit
    // `fn -> impl Future + Send` shape, which is also what the macro emits.
    #[allow(clippy::manual_async_fn)]
    impl Model for BrokenModel {
        type Pk = i64;
        type Fields = BrokenFields;

        fn table_name() -> &'static str {
            "broken_lateral_models"
        }

        fn pk_value(&self) -> &Self::Pk {
            unreachable!("not used in lateral tests")
        }

        fn descriptor() -> &'static ModelDescriptor {
            unreachable!("not used in lateral tests")
        }

        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: Self::Pk,
        ) -> impl Future<Output = crate::Result<Self>> + Send {
            async { unreachable!("not used in lateral tests") }
        }

        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl Future<Output = crate::Result<Self>> + Send {
            async { unreachable!("not used in lateral tests") }
        }

        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl Future<Output = crate::Result<()>> + Send + 'ctx {
            async { unreachable!("not used in lateral tests") }
        }

        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl Future<Output = crate::Result<()>> + Send {
            async { unreachable!("not used in lateral tests") }
        }

        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl Future<Output = crate::Result<Self>> + Send + 'ctx {
            async { unreachable!("not used in lateral tests") }
        }
    }

    impl FromPgRow for BrokenModel {
        const COLUMNS: &'static [&'static str] = &["score"];
        const COLUMN_LIST: &'static str = "score";

        fn from_pg_row(_row: &tokio_postgres::Row) -> crate::Result<Self> {
            unreachable!("not used in lateral tests")
        }
    }

    impl FromJoinedPgRow for BrokenModel {
        fn from_joined_pg_row(_row: &tokio_postgres::Row, _prefix: &str) -> crate::Result<Self> {
            unreachable!("not used in lateral tests")
        }
    }

    #[test]
    fn lateral_inner_predicate_lowering_failure_maps_to_predicate() {
        let outer = BrokenModel::objects();
        let inner = BrokenModel::objects().filter(|f| f.score().eq(1));

        let err = outer
            .join_lateral(inner)
            .__sql_for_test()
            .expect_err("unsupported portable predicate must fail SQL emission");

        assert!(
            matches!(err, crate::DjogiError::Predicate(_)),
            "expected predicate error, got {err:?}"
        );
    }

    #[test]
    fn lateral_outer_predicate_lowering_failure_maps_to_predicate() {
        let outer = BrokenModel::objects().filter(|f| f.score().eq(1));
        let inner = BrokenModel::objects();

        let err = outer
            .join_lateral(inner)
            .__sql_for_test()
            .expect_err("unsupported portable predicate must fail SQL emission");

        assert!(
            matches!(err, crate::DjogiError::Predicate(_)),
            "expected predicate error, got {err:?}"
        );
    }
}
