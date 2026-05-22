//! Typed `MERGE INTO ... USING ...` query surface.
//!
//! PostgreSQL 18+ only.
//!
//! # What
//!
//! `MERGE` is a single statement that can perform `INSERT`, `UPDATE`, or `DELETE`
//! operations on a target table based on whether rows match a source relation.
//!
//! # Why
//!
//! Adopters who need to perform upserts, "update if changed" guards, or
//! soft-delete missing source rows previously had to drop to raw SQL.
//!
//! # How
//!
//! ```ignore
//! use djogi::prelude::*;
//!
//! source_qs.merge_into::<Target, _, _>(|target, source| {
//!     target.external_id().merge_on_eq(source.external_id())
//! })
//! .when_matched_and_update(Some(target.payload().is_distinct_from_source(source.payload())), vec![
//!     target.payload().merge_copy_from(source.payload()),
//! ])
//! .when_not_matched_then_insert(None, vec![
//!     target.external_id().merge_insert_from(source.external_id()),
//!     target.payload().merge_insert_from(source.payload()),
//! ])
//! .execute(&ctx)
//! .await?;
//! ```

use crate::DjogiError;
use crate::context::DjogiContext;
use crate::expr::node::ExprNode;
use crate::model::Model;
use crate::pg::accumulator::as_params;
use crate::pg::decode::FromPgRow;
use crate::query::field::{DjogiField, FieldRef};
use crate::query::queryset::QuerySet;
use crate::query::sql::build_merge;
use crate::query::terminal::auto_set_tenant;
use std::collections::HashSet;
use std::marker::PhantomData;

/// Result count for a `MERGE` operation.
///
/// PostgreSQL returns the total number of rows affected across all actions
/// in the command tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct MergeCounts {
    pub total_affected: u64,
}

impl MergeCounts {
    pub fn zero() -> Self {
        Self::default()
    }
}

/// A `MERGE INTO` statement.
pub struct MergeStmt<S: Model + FromPgRow, T: Model> {
    pub(crate) source: QuerySet<S>,
    pub(crate) on: Vec<MergeOnEq<S, T>>,
    pub(crate) branches: Vec<MergeBranch<S, T>>,
    pub(crate) _target: PhantomData<T>,
}

impl<S: Model + FromPgRow, T: Model> MergeStmt<S, T> {
    /// Add a `WHEN MATCHED [AND condition] THEN UPDATE SET ...` branch.
    pub fn when_matched_and_update<C, U>(mut self, condition: Option<C>, updates: U) -> Self
    where
        C: IntoMergeWhenCondition<S, T>,
        U: IntoMergeUpdates<S, T>,
    {
        self.branches.push(MergeBranch {
            match_kind: MergeMatchKind::Matched,
            condition: condition.map(|c| c.into_condition()),
            action: MergeAction::<S, T>::Update(updates.into_merge_updates()),
        });
        self
    }

    /// Add a `WHEN MATCHED [AND condition] THEN DELETE` branch.
    pub fn when_matched_and_delete<C>(mut self, condition: Option<C>) -> Self
    where
        C: IntoMergeWhenCondition<S, T>,
    {
        self.branches.push(MergeBranch {
            match_kind: MergeMatchKind::Matched,
            condition: condition.map(|c| c.into_condition()),
            action: MergeAction::<S, T>::Delete,
        });
        self
    }

    /// Add a `WHEN NOT MATCHED [BY TARGET] [AND condition] THEN INSERT (...) VALUES (...)` branch.
    pub fn when_not_matched_then_insert<C, I>(mut self, condition: Option<C>, columns: I) -> Self
    where
        C: IntoMergeWhenCondition<S, T>,
        I: IntoMergeInsertColumns<S, T>,
    {
        self.branches.push(MergeBranch {
            match_kind: MergeMatchKind::NotMatchedByTarget,
            condition: condition.map(|c| c.into_condition()),
            action: MergeAction::<S, T>::Insert(columns.into_merge_insert_columns()),
        });
        self
    }

    /// Add a `WHEN NOT MATCHED BY SOURCE [AND condition] THEN UPDATE SET ...` branch.
    pub fn when_not_matched_by_source_then_update<C, U>(
        mut self,
        condition: Option<C>,
        updates: U,
    ) -> Self
    where
        C: IntoMergeWhenCondition<S, T>,
        U: IntoMergeUpdates<S, T>,
    {
        self.branches.push(MergeBranch {
            match_kind: MergeMatchKind::NotMatchedBySource,
            condition: condition.map(|c| c.into_condition()),
            action: MergeAction::<S, T>::Update(updates.into_merge_updates()),
        });
        self
    }

    /// Add a `WHEN NOT MATCHED BY SOURCE [AND condition] THEN DELETE` branch.
    pub fn when_not_matched_by_source_then_delete<C>(mut self, condition: Option<C>) -> Self
    where
        C: IntoMergeWhenCondition<S, T>,
    {
        self.branches.push(MergeBranch {
            match_kind: MergeMatchKind::NotMatchedBySource,
            condition: condition.map(|c| c.into_condition()),
            action: MergeAction::<S, T>::Delete,
        });
        self
    }

    /// Convenience for `WHEN MATCHED AND (target.col IS DISTINCT FROM source.col OR ...) THEN UPDATE SET ...`.
    ///
    /// Automatically builds a condition that only updates rows where at least one of the
    /// mapped columns has changed. Only columns mapped with `merge_copy_from` are
    /// included in the change check.
    pub fn when_matched_update_changed<U>(self, updates: U) -> Self
    where
        U: IntoMergeUpdates<S, T> + Clone,
    {
        let upds = updates.clone().into_merge_updates();
        let mut condition: Option<MergeWhenCondition<S, T>> = None;

        for update in &upds {
            if let MergeValue::SourceField(source_col, _) = &update.value {
                let cond = MergeWhenCondition {
                    node: ExprNode::Cmp {
                        op: crate::expr::node::CmpOp::IsDistinctFrom,
                        lhs: Box::new(ExprNode::Field {
                            column: update.target_col,
                        }),
                        rhs: Box::new(ExprNode::OuterRefColumn {
                            table: "__djogi_src",
                            column: source_col,
                        }),
                    },
                    _marker: PhantomData,
                };

                condition = match condition {
                    Some(c) => Some(c.or(cond)),
                    None => Some(cond),
                };
            }
        }

        self.when_matched_and_update(condition, updates)
    }

    pub async fn execute(self, ctx: &mut DjogiContext) -> Result<MergeCounts, DjogiError> {
        // 1. Validation before short-circuit
        self.validate()?;

        // 2. Short-circuit: structural-empty source.
        // If there are BY SOURCE branches, we cannot short-circuit because an empty
        // source means all target rows are NOT MATCHED BY SOURCE.
        let has_by_source = self
            .branches
            .iter()
            .any(|b| b.match_kind == MergeMatchKind::NotMatchedBySource);
        if self.source.is_empty() && !has_by_source {
            return Ok(MergeCounts::zero());
        }

        // 3. Tenant / RLS auto-set (Target then Source)
        auto_set_tenant::<T>(ctx).await?;
        auto_set_tenant::<S>(ctx).await?;

        // 4. SQL Assembly
        let acc = build_merge::<S, T>(&self.source, &self.on, &self.branches, None)
            .map_err(DjogiError::from)?;

        // 5. Execution
        let (sql, binds) = acc.into_parts();
        let params = as_params(&binds);
        let rows_affected = ctx.execute(&sql, &params).await?;

        Ok(MergeCounts {
            total_affected: rows_affected,
        })
    }

    fn validate(&self) -> Result<(), DjogiError> {
        let target_table = T::table_name();
        if self.on.is_empty() {
            return Err(DjogiError::MergeNoBranches {
                table: target_table,
                reason: "MERGE requires at least one ON condition".to_string(),
            });
        }
        if self.branches.is_empty() {
            return Err(DjogiError::MergeNoBranches {
                table: target_table,
                reason: "MERGE requires at least one WHEN branch".to_string(),
            });
        }

        // 1. Reject incompatible source state.
        // MERGE USING subquery does not support Djogi-side prefetch/select_related/cache.
        // Locks (FOR UPDATE) inside the USING subquery are rejected by PG.
        if !self.source.prefetch_paths.is_empty() {
            return Err(DjogiError::MergeSourceInvalid {
                table: S::table_name(),
                reason: "prefetch is not supported in MERGE source",
            });
        }
        if !self.source.select_related_paths.is_empty() {
            return Err(DjogiError::MergeSourceInvalid {
                table: S::table_name(),
                reason: "select_related is not supported in MERGE source",
            });
        }
        if self.source.cache_target.is_some() {
            return Err(DjogiError::MergeSourceInvalid {
                table: S::table_name(),
                reason: "cache-bound querysets are not supported in MERGE source",
            });
        }
        if self.source.lock != crate::query::lock::LockMode::None {
            return Err(DjogiError::MergeSourceInvalid {
                table: S::table_name(),
                reason: "lock (FOR UPDATE/SHARE) is not supported in MERGE source subquery",
            });
        }
        if self.source.distinct != crate::query::queryset::DistinctMode::None {
            return Err(DjogiError::MergeSourceInvalid {
                table: S::table_name(),
                reason: "DISTINCT is not supported in MERGE source",
            });
        }

        // 2. Branch-level validations.
        let mut seen_unconditional_matched = false;
        let mut seen_unconditional_not_matched = false;
        let mut seen_unconditional_not_matched_by_source = false;

        for (i, branch) in self.branches.iter().enumerate() {
            let is_unconditional = branch.condition.is_none();

            // Unreachable branch check: same-kind unconditional branch follows another.
            match branch.match_kind {
                MergeMatchKind::Matched => {
                    if seen_unconditional_matched {
                        return Err(DjogiError::MergeBranchInvalid {
                            table: target_table,
                            reason: format!(
                                "branch {} (WHEN MATCHED) is unreachable: a previous unconditional MATCHED branch already covers all cases",
                                i + 1
                            ),
                        });
                    }
                    if is_unconditional {
                        seen_unconditional_matched = true;
                    }
                }
                MergeMatchKind::NotMatchedByTarget => {
                    if seen_unconditional_not_matched {
                        return Err(DjogiError::MergeBranchInvalid {
                            table: target_table,
                            reason: format!(
                                "branch {} (WHEN NOT MATCHED) is unreachable: a previous unconditional NOT MATCHED branch already covers all cases",
                                i + 1
                            ),
                        });
                    }
                    if is_unconditional {
                        seen_unconditional_not_matched = true;
                    }
                }
                MergeMatchKind::NotMatchedBySource => {
                    if seen_unconditional_not_matched_by_source {
                        return Err(DjogiError::MergeBranchInvalid {
                            table: target_table,
                            reason: format!(
                                "branch {} (WHEN NOT MATCHED BY SOURCE) is unreachable: a previous unconditional NOT MATCHED BY SOURCE branch already covers all cases",
                                i + 1
                            ),
                        });
                    }
                    if is_unconditional {
                        seen_unconditional_not_matched_by_source = true;
                    }
                }
            }

            // Action validations.
            match &branch.action {
                MergeAction::Update(updates) => {
                    let mut cols = HashSet::new();
                    for update in updates {
                        if update.target_col == "updated_at" {
                            return Err(DjogiError::MergeBranchInvalid {
                                table: target_table,
                                reason: format!(
                                    "branch {}: manual assignment to `updated_at` is rejected; MERGE always auto-stamps updated_at = now()",
                                    i + 1
                                ),
                            });
                        }
                        if !cols.insert(update.target_col) {
                            return Err(DjogiError::MergeBranchInvalid {
                                table: target_table,
                                reason: format!(
                                    "branch {}: duplicate target column `{}` in UPDATE action",
                                    i + 1,
                                    update.target_col
                                ),
                            });
                        }
                    }
                }
                MergeAction::Insert(columns) => {
                    let mut cols = HashSet::new();
                    for col in columns {
                        if !cols.insert(col.target_col) {
                            return Err(DjogiError::MergeBranchInvalid {
                                table: target_table,
                                reason: format!(
                                    "branch {}: duplicate target column `{}` in INSERT action",
                                    i + 1,
                                    col.target_col
                                ),
                            });
                        }
                    }
                }
                MergeAction::Delete => {}
                MergeAction::_Marker(_) => {
                    unreachable!("MergeAction::_Marker is a type marker only")
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct MergeBranch<S: Model, T: Model> {
    pub(crate) match_kind: MergeMatchKind,
    pub(crate) condition: Option<MergeWhenCondition<S, T>>,
    pub(crate) action: MergeAction<S, T>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMatchKind {
    Matched,
    NotMatchedByTarget,
    NotMatchedBySource,
}

#[derive(Debug, Clone)]
pub enum MergeAction<S: Model, T: Model> {
    Update(Vec<MergeUpdateAssignment<S, T>>),
    Delete,
    Insert(Vec<MergeInsertColumn<S, T>>),
    /// Marker to satisfy unused type parameters.
    _Marker(PhantomData<(S, T)>),
}

#[derive(Debug, Clone)]
pub struct MergeOnEq<S: Model, T: Model> {
    pub(crate) target_col: &'static str,
    pub(crate) source_col: &'static str,
    pub(crate) _marker: PhantomData<(S, T)>,
}

#[derive(Debug, Clone)]
pub struct MergeWhenCondition<S: Model, T: Model> {
    pub(crate) node: ExprNode,
    pub(crate) _marker: PhantomData<(S, T)>,
}

impl<S: Model, T: Model> MergeWhenCondition<S, T> {
    pub fn and(self, other: Self) -> Self {
        Self {
            node: ExprNode::And(Box::new(self.node), Box::new(other.node)),
            _marker: PhantomData,
        }
    }

    pub fn or(self, other: Self) -> Self {
        Self {
            node: ExprNode::Or(Box::new(self.node), Box::new(other.node)),
            _marker: PhantomData,
        }
    }
}

impl<S: Model + FromPgRow, T: Model> std::ops::Not for MergeWhenCondition<S, T> {
    type Output = Self;

    fn not(self) -> Self::Output {
        Self {
            node: ExprNode::Not(Box::new(self.node)),
            _marker: PhantomData,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MergeUpdateAssignment<S: Model, T: Model> {
    pub(crate) target_col: &'static str,
    pub(crate) value: MergeValue<S, T>,
    pub(crate) _marker: PhantomData<(S, T)>,
}

#[derive(Debug, Clone)]
pub struct MergeInsertColumn<S: Model, T: Model> {
    pub(crate) target_col: &'static str,
    pub(crate) value: MergeValue<S, T>,
    pub(crate) _marker: PhantomData<(S, T)>,
}

#[derive(Debug, Clone)]
pub(crate) enum MergeValue<S: Model, T: Model> {
    Literal(crate::query::condition::FilterValue, PhantomData<(S, T)>),
    SourceField(&'static str, PhantomData<(S, T)>),
    TargetExpr(crate::expr::node::ExprNode, PhantomData<(S, T)>),
}

pub trait IntoMergeWhenCondition<S: Model, T: Model> {
    fn into_condition(self) -> MergeWhenCondition<S, T>;
}

impl<S: Model, T: Model> IntoMergeWhenCondition<S, T> for MergeWhenCondition<S, T> {
    fn into_condition(self) -> MergeWhenCondition<S, T> {
        self
    }
}

pub trait IntoMergeUpdates<S: Model, T: Model> {
    fn into_merge_updates(self) -> Vec<MergeUpdateAssignment<S, T>>;
}

impl<S: Model, T: Model> IntoMergeUpdates<S, T> for Vec<MergeUpdateAssignment<S, T>> {
    fn into_merge_updates(self) -> Vec<MergeUpdateAssignment<S, T>> {
        self
    }
}

impl<S: Model, T: Model> IntoMergeUpdates<S, T> for MergeUpdateAssignment<S, T> {
    fn into_merge_updates(self) -> Vec<MergeUpdateAssignment<S, T>> {
        vec![self]
    }
}

pub trait IntoMergeInsertColumns<S: Model, T: Model> {
    fn into_merge_insert_columns(self) -> Vec<MergeInsertColumn<S, T>>;
}

impl<S: Model, T: Model> IntoMergeInsertColumns<S, T> for Vec<MergeInsertColumn<S, T>> {
    fn into_merge_insert_columns(self) -> Vec<MergeInsertColumn<S, T>> {
        self
    }
}

impl<S: Model, T: Model> IntoMergeInsertColumns<S, T> for MergeInsertColumn<S, T> {
    fn into_merge_insert_columns(self) -> Vec<MergeInsertColumn<S, T>> {
        vec![self]
    }
}

pub trait IntoMergeOn<S: Model, T: Model> {
    fn into_merge_on(self) -> Vec<MergeOnEq<S, T>>;
}

impl<S: Model, T: Model> IntoMergeOn<S, T> for MergeOnEq<S, T> {
    fn into_merge_on(self) -> Vec<MergeOnEq<S, T>> {
        vec![self]
    }
}

impl<S: Model, T: Model> IntoMergeOn<S, T> for Vec<MergeOnEq<S, T>> {
    fn into_merge_on(self) -> Vec<MergeOnEq<S, T>> {
        self
    }
}

impl<T: Model, V> FieldRef<T, V> {
    /// Bind this target column to a source column for the `MERGE ... ON` clause.
    pub fn merge_on_eq<S: Model>(self, source: FieldRef<S, V>) -> MergeOnEq<S, T> {
        MergeOnEq {
            target_col: self.column(),
            source_col: source.column(),
            _marker: PhantomData,
        }
    }

    /// Bind this target column to a source column for a `MERGE` update action.
    pub fn merge_copy_from<S: Model>(self, source: FieldRef<S, V>) -> MergeUpdateAssignment<S, T> {
        MergeUpdateAssignment {
            target_col: self.column(),
            value: MergeValue::SourceField(source.column(), PhantomData),
            _marker: PhantomData,
        }
    }

    /// Bind this target column to a source column for a `MERGE` insert action.
    pub fn merge_insert_from<S: Model>(self, source: FieldRef<S, V>) -> MergeInsertColumn<S, T> {
        MergeInsertColumn {
            target_col: self.column(),
            value: MergeValue::SourceField(source.column(), PhantomData),
            _marker: PhantomData,
        }
    }

    /// Bind this target column to a literal value for a `MERGE` update action.
    pub fn merge_set<S: Model>(self, value: V) -> MergeUpdateAssignment<S, T>
    where
        V: crate::query::field::IntoFilterValue,
    {
        MergeUpdateAssignment {
            target_col: self.column(),
            value: MergeValue::Literal(value.into_filter_value(), PhantomData),
            _marker: PhantomData,
        }
    }

    /// Bind this target column to a literal value for a `MERGE` insert action.
    pub fn merge_insert_value<S: Model>(self, value: V) -> MergeInsertColumn<S, T>
    where
        V: crate::query::field::IntoFilterValue,
    {
        MergeInsertColumn {
            target_col: self.column(),
            value: MergeValue::Literal(value.into_filter_value(), PhantomData),
            _marker: PhantomData,
        }
    }

    /// Bind this target column to an expression for a `MERGE` update action.
    pub fn merge_set_expr<S: Model, E>(self, value: E) -> MergeUpdateAssignment<S, T>
    where
        E: Into<crate::expr::Expr<V>>,
    {
        let expr = value.into();
        MergeUpdateAssignment {
            target_col: self.column(),
            value: MergeValue::TargetExpr(expr.node, PhantomData),
            _marker: PhantomData,
        }
    }

    /// Bind this target column to an expression for a `MERGE` insert action.
    pub fn merge_insert_expr<S: Model, E>(self, value: E) -> MergeInsertColumn<S, T>
    where
        E: Into<crate::expr::Expr<V>>,
    {
        let expr = value.into();
        MergeInsertColumn {
            target_col: self.column(),
            value: MergeValue::TargetExpr(expr.node, PhantomData),
            _marker: PhantomData,
        }
    }

    /// `target.col IS DISTINCT FROM source.col` condition for `WHEN MATCHED AND ...`.
    pub fn is_distinct_from_source<S: Model>(
        self,
        source: FieldRef<S, V>,
    ) -> MergeWhenCondition<S, T> {
        MergeWhenCondition {
            node: ExprNode::Cmp {
                op: crate::expr::node::CmpOp::IsDistinctFrom,
                lhs: Box::new(ExprNode::Field {
                    column: self.column(),
                }),
                rhs: Box::new(ExprNode::OuterRefColumn {
                    table: "__djogi_src",
                    column: source.column(),
                }),
            },
            _marker: PhantomData,
        }
    }
}

impl<T: Model, V> DjogiField<T, V> {
    pub fn merge_on_eq<S: Model>(self, source: DjogiField<S, V>) -> MergeOnEq<S, T> {
        self.sql.merge_on_eq(source.sql)
    }

    pub fn merge_copy_from<S: Model>(
        self,
        source: DjogiField<S, V>,
    ) -> MergeUpdateAssignment<S, T> {
        self.sql.merge_copy_from(source.sql)
    }

    pub fn merge_insert_from<S: Model>(self, source: DjogiField<S, V>) -> MergeInsertColumn<S, T> {
        self.sql.merge_insert_from(source.sql)
    }

    pub fn merge_set<S: Model>(self, value: V) -> MergeUpdateAssignment<S, T>
    where
        V: crate::query::field::IntoFilterValue,
    {
        self.sql.merge_set(value)
    }

    pub fn merge_insert_value<S: Model>(self, value: V) -> MergeInsertColumn<S, T>
    where
        V: crate::query::field::IntoFilterValue,
    {
        self.sql.merge_insert_value(value)
    }

    pub fn merge_set_expr<S: Model, E>(self, value: E) -> MergeUpdateAssignment<S, T>
    where
        E: Into<crate::expr::Expr<V>>,
    {
        self.sql.merge_set_expr(value)
    }

    pub fn merge_insert_expr<S: Model, E>(self, value: E) -> MergeInsertColumn<S, T>
    where
        E: Into<crate::expr::Expr<V>>,
    {
        self.sql.merge_insert_expr(value)
    }

    pub fn is_distinct_from_source<S: Model>(
        self,
        source: DjogiField<S, V>,
    ) -> MergeWhenCondition<S, T> {
        self.sql.is_distinct_from_source(source.sql)
    }
}

impl<S: Model + FromPgRow> QuerySet<S> {
    /// Enter a `MERGE INTO` statement with this queryset as the source.
    pub fn merge_into<T, F, O>(self, on_f: F) -> MergeStmt<S, T>
    where
        T: Model,
        F: FnOnce(T::Fields, S::Fields) -> O,
        O: IntoMergeOn<S, T>,
    {
        let on = on_f(T::Fields::default(), S::Fields::default()).into_merge_on();
        MergeStmt {
            source: self,
            on,
            branches: Vec::new(),
            _target: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::ModelDescriptor;

    use crate::pg::decode::FromPgRow;

    #[derive(Clone)]
    struct Fake;
    impl crate::model::__sealed::Sealed for Fake {}
    #[allow(clippy::manual_async_fn)]
    impl Model for Fake {
        type Pk = i64;
        type Fields = FakeFields;
        fn table_name() -> &'static str {
            "fake"
        }
        fn pk_value(&self) -> &i64 {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            unreachable!()
        }
        fn get(
            _ctx: &mut DjogiContext,
            _id: i64,
        ) -> impl std::future::Future<Output = Result<Self, DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create(
            _ctx: &mut DjogiContext,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), DjogiError>> + Send + 'ctx {
            async { unreachable!() }
        }
        fn delete(
            self,
            _ctx: &mut DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut DjogiContext,
        ) -> impl std::future::Future<Output = Result<Self, DjogiError>> + Send + 'ctx {
            async { unreachable!() }
        }
    }

    impl FromPgRow for Fake {
        const COLUMNS: &'static [&'static str] = &["id", "created_at", "updated_at", "payload"];
        const COLUMN_LIST: &'static str = "id, created_at, updated_at, payload";
        fn from_pg_row(_row: &tokio_postgres::Row) -> Result<Self, DjogiError> {
            unreachable!()
        }
    }

    #[derive(Default, Clone, Copy)]
    struct FakeFields;
    impl FakeFields {
        fn id(self) -> FieldRef<Fake, i64> {
            FieldRef::new("id")
        }
        fn payload(self) -> FieldRef<Fake, String> {
            FieldRef::new("payload")
        }
    }

    impl Fake {
        fn fields() -> FakeFields {
            FakeFields
        }
    }

    #[test]
    fn merge_basic_sql_emission() {
        let source: QuerySet<Fake> = QuerySet::new();
        let stmt = source
            .merge_into::<Fake, _, _>(|tgt, src| tgt.id().merge_on_eq(src.id()))
            .when_matched_and_update(
                None::<MergeWhenCondition<Fake, Fake>>,
                vec![
                    Fake::fields()
                        .payload()
                        .merge_copy_from(Fake::fields().payload()),
                ],
            )
            .when_not_matched_then_insert(
                None::<MergeWhenCondition<Fake, Fake>>,
                vec![
                    Fake::fields().id().merge_insert_from(Fake::fields().id()),
                    Fake::fields()
                        .payload()
                        .merge_insert_from(Fake::fields().payload()),
                ],
            );

        let acc = build_merge(&stmt.source, &stmt.on, &stmt.branches, None).unwrap();
        let sql = acc.sql();

        assert!(sql.contains("MERGE INTO fake AS tgt"));
        assert!(sql.contains(
            "USING (SELECT id, created_at, updated_at, payload FROM fake) AS __djogi_src"
        ));
        assert!(sql.contains("ON tgt.id = __djogi_src.id"));
        assert!(sql.contains(
            "WHEN MATCHED THEN UPDATE SET updated_at = now(), payload = __djogi_src.payload"
        ));
        assert!(sql.contains("WHEN NOT MATCHED THEN INSERT (id, payload) VALUES (__djogi_src.id, __djogi_src.payload)"));
    }

    #[test]
    fn merge_update_changed_sql_emission() {
        let source: QuerySet<Fake> = QuerySet::new();
        let stmt = source
            .merge_into::<Fake, _, _>(|tgt, src| tgt.id().merge_on_eq(src.id()))
            .when_matched_update_changed(vec![
                Fake::fields()
                    .payload()
                    .merge_copy_from(Fake::fields().payload()),
            ]);

        let acc = build_merge(&stmt.source, &stmt.on, &stmt.branches, None).unwrap();
        let sql = acc.sql();

        assert!(sql.contains(
            "WHEN MATCHED AND tgt.payload IS DISTINCT FROM __djogi_src.payload THEN UPDATE SET"
        ));
    }

    #[test]
    fn merge_validation_rejects_empty_on() {
        let source: QuerySet<Fake> = QuerySet::new();
        let stmt = MergeStmt::<Fake, Fake> {
            source,
            on: vec![],
            branches: vec![],
            _target: PhantomData,
        };
        let res = stmt.validate();
        assert!(matches!(res, Err(DjogiError::MergeNoBranches { .. })));
    }

    #[test]
    fn merge_validation_rejects_duplicate_columns() {
        let source: QuerySet<Fake> = QuerySet::new();
        let stmt = source
            .merge_into::<Fake, _, _>(|tgt, src| tgt.id().merge_on_eq(src.id()))
            .when_matched_and_update(
                None::<MergeWhenCondition<Fake, Fake>>,
                vec![
                    Fake::fields().payload().merge_set("a".to_string()),
                    Fake::fields().payload().merge_set("b".to_string()),
                ],
            );

        let res = stmt.validate();
        assert!(matches!(res, Err(DjogiError::MergeBranchInvalid { .. })));
        if let Err(DjogiError::MergeBranchInvalid { reason, .. }) = res {
            assert!(reason.contains("duplicate target column `payload`"));
        }
    }
}
