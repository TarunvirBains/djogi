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
use crate::query::queryset::DistinctMode;
use crate::query::queryset::QuerySet;
use crate::query::sql::push_tail_qualified;
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

    pub(crate) fn build_sql(&self, is_left: bool, is_count: bool) -> Result<SqlAccumulator> {
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

        acc.push_sql(" FROM ");
        acc.push_sql(L::table_name());
        acc.push_sql(" AS ");
        acc.push_sql(LEFT_ALIAS);

        if is_left {
            acc.push_sql(" LEFT JOIN LATERAL (");
        } else {
            acc.push_sql(" JOIN LATERAL (");
        }

        // Inner lateral query
        let mut inner_acc = SqlAccumulator::new("");
        match &self.inner.distinct {
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
        push_tail_qualified(&mut inner_acc, &self.inner, None)
            .map_err(|e| DjogiError::Validation(e.to_string()))?; // inner modifiers

        acc.extend_with(inner_acc);

        acc.push_sql(") AS ");
        acc.push_sql(RIGHT_ALIAS);
        acc.push_sql(" ON TRUE");

        // Outer modifiers
        let mut outer_acc = SqlAccumulator::new("");

        // If we are building COUNT, we strip outer ORDER BY, LIMIT, OFFSET, but we keep WHERE.
        if is_count {
            let mut shadow = QuerySet::<L>::new();
            shadow.condition = self.outer.condition.clone();
            push_tail_qualified(&mut outer_acc, &shadow, Some(LEFT_ALIAS))
                .map_err(|e| DjogiError::Validation(e.to_string()))?;
        } else {
            push_tail_qualified(&mut outer_acc, &self.outer, Some(LEFT_ALIAS))
                .map_err(|e| DjogiError::Validation(e.to_string()))?;
        }

        acc.extend_with(outer_acc);

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
        let acc = self.build_sql(false, false)?;
        let (sql, _binds) = acc.into_parts();
        Ok(sql)
    }

    /// Build the lateral COUNT SQL this queryset would execute, without
    /// touching a database. **Internal-test plumbing — never call
    /// this from adopter code.**
    #[doc(hidden)]
    pub fn __count_sql_for_test(&self) -> Result<String> {
        let acc = self.build_sql(false, true)?;
        let (sql, _binds) = acc.into_parts();
        Ok(sql)
    }

    pub async fn count(self, ctx: &mut DjogiContext) -> Result<i64> {
        if self.outer.is_empty || self.inner.is_empty {
            return Ok(0);
        }
        self.validate()?;
        auto_set_tenant::<L>(ctx).await?;
        auto_set_tenant::<R>(ctx).await?;

        let acc = self.build_sql(false, true)?;
        let (sql, binds) = acc.into_parts();
        let params = as_params(&binds);
        let row = ctx.query_one(&sql, &params).await?;
        Ok(row.get(0))
    }

    pub async fn fetch_all(self, ctx: &mut DjogiContext) -> Result<Vec<(L, R)>> {
        if self.outer.is_empty || self.inner.is_empty {
            return Ok(Vec::new());
        }
        self.validate()?;
        auto_set_tenant::<L>(ctx).await?;
        auto_set_tenant::<R>(ctx).await?;

        let acc = self.build_sql(false, false)?;
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

    pub async fn first(mut self, ctx: &mut DjogiContext) -> Result<Option<(L, R)>> {
        self.outer.limit = Some(1);
        let mut all = self.fetch_all(ctx).await?;
        Ok(all.pop())
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
        let acc = self.build_sql(true, false)?;
        let (sql, _binds) = acc.into_parts();
        Ok(sql)
    }

    /// Build the lateral COUNT SQL this queryset would execute, without
    /// touching a database. **Internal-test plumbing — never call
    /// this from adopter code.**
    #[doc(hidden)]
    pub fn __count_sql_for_test(&self) -> Result<String> {
        let acc = self.build_sql(true, true)?;
        let (sql, _binds) = acc.into_parts();
        Ok(sql)
    }

    pub async fn count(self, ctx: &mut DjogiContext) -> Result<i64> {
        if self.outer.is_empty {
            return Ok(0);
        }
        self.validate()?;
        auto_set_tenant::<L>(ctx).await?;
        auto_set_tenant::<R>(ctx).await?;

        let acc = self.build_sql(true, true)?;
        let (sql, binds) = acc.into_parts();
        let params = as_params(&binds);
        let row = ctx.query_one(&sql, &params).await?;
        Ok(row.get(0))
    }

    pub async fn fetch_all(self, ctx: &mut DjogiContext) -> Result<Vec<(L, Option<R>)>> {
        if self.outer.is_empty {
            return Ok(Vec::new());
        }
        self.validate()?;
        auto_set_tenant::<L>(ctx).await?;
        auto_set_tenant::<R>(ctx).await?;

        let acc = self.build_sql(true, false)?;
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

    pub async fn first(mut self, ctx: &mut DjogiContext) -> Result<Option<(L, Option<R>)>> {
        self.outer.limit = Some(1);
        let mut all = self.fetch_all(ctx).await?;
        Ok(all.pop())
    }
}
