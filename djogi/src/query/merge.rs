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

/// Framework-fixed alias for the target table in `MERGE`.
pub(crate) const TGT_ALIAS: &str = "tgt";
/// Framework-fixed alias for the source subquery in `MERGE`.
pub(crate) const SRC_ALIAS: &str = "__djogi_src";

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

impl<S: Model + FromPgRow, T: Model> Clone for MergeStmt<S, T> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            on: self.on.clone(),
            branches: self.branches.clone(),
            _target: PhantomData,
        }
    }
}

impl<S: Model + FromPgRow, T: Model> std::fmt::Debug for MergeStmt<S, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MergeStmt")
            .field("source", &self.source)
            .field("on", &self.on)
            .field("branches", &self.branches)
            .finish()
    }
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
                            table: SRC_ALIAS,
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

        // Safety rail: if no `merge_copy_from` mappings were provided, this helper
        // must not silently degrade into an unconditional MATCHED update.
        // Force the branch predicate to `FALSE` so the statement becomes a no-op
        // for MATCHED rows instead of broad-updating unexpectedly.
        let condition = condition.or_else(|| {
            Some(MergeWhenCondition {
                node: ExprNode::Literal(crate::query::condition::FilterValue::Bool(false)),
                _marker: PhantomData,
            })
        });

        self.when_matched_and_update(condition, updates)
    }

    pub async fn execute(self, ctx: &mut DjogiContext) -> Result<MergeCounts, DjogiError> {
        // 1. Validation before short-circuit
        self.validate()?;

        // 2. Short-circuit: structural-empty source.
        // If there are BY SOURCE branches, we cannot short-circuit because an empty
        // source means all target rows are NOT MATCHED BY SOURCE.
        //
        // However, structural-empty source (`.none()`) with BY SOURCE branches is
        // rejected in validate() to prevent unintentional broad updates.
        if self.source.is_empty() {
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

        // Reject structural-empty source with BY SOURCE branches by default (Spec #178 BLOCK 1).
        let has_by_source = self
            .branches
            .iter()
            .any(|b| b.match_kind == MergeMatchKind::NotMatchedBySource);
        if self.source.is_empty() && has_by_source {
            return Err(DjogiError::MergeSourceInvalid {
                table: S::table_name(),
                reason: "structural-empty source (.none()) is rejected when BY SOURCE branches exist to prevent broad updates; use an explicit filter that matches no rows if this was intentional",
            });
        }

        // 2. Branch-level validations.
        let mut seen_unconditional_matched = false;
        let mut seen_unconditional_not_matched = false;
        let mut seen_unconditional_not_matched_by_source = false;

        for (i, branch) in self.branches.iter().enumerate() {
            let is_unconditional = branch.condition.is_none();

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

            // BY SOURCE predicates must only reference target fields (Spec #178 BLOCK 2).
            if branch.match_kind == MergeMatchKind::NotMatchedBySource
                && let Some(cond) = &branch.condition
            {
                check_target_only_predicate(&cond.node, target_table)?;
            }
            if branch.match_kind == MergeMatchKind::NotMatchedByTarget
                && let Some(cond) = &branch.condition
            {
                check_source_only_predicate(&cond.node, target_table)?;
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
                        if branch.match_kind == MergeMatchKind::NotMatchedBySource {
                            check_by_source_update_value(&update.value, target_table)?;
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
                        if branch.match_kind == MergeMatchKind::NotMatchedByTarget {
                            check_not_matched_insert_value(&col.value, target_table)?;
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

fn check_by_source_update_value<S: Model, T: Model>(
    value: &MergeValue<S, T>,
    table: &'static str,
) -> Result<(), DjogiError> {
    match value {
        MergeValue::SourceField(column, _) => Err(DjogiError::MergeBranchInvalid {
            table,
            reason: format!(
                "BY SOURCE branch update cannot reference source field `{}`",
                column
            ),
        }),
        MergeValue::TargetExpr(node, _) => check_target_only_predicate(node, table),
        MergeValue::Literal(_, _) => Ok(()),
    }
}

fn check_not_matched_insert_value<S: Model, T: Model>(
    value: &MergeValue<S, T>,
    table: &'static str,
) -> Result<(), DjogiError> {
    match value {
        MergeValue::TargetExpr(node, _) => check_source_only_predicate(node, table),
        MergeValue::SourceField(_, _) | MergeValue::Literal(_, _) => Ok(()),
    }
}

fn check_target_only_predicate(node: &ExprNode, table: &'static str) -> Result<(), DjogiError> {
    match node {
        ExprNode::Field { .. } => Ok(()),
        ExprNode::OuterRefColumn {
            table: alias,
            column,
        } => {
            if *alias == SRC_ALIAS {
                return Err(DjogiError::MergeBranchInvalid {
                    table,
                    reason: format!(
                        "BY SOURCE branch condition cannot reference source field `{}`",
                        column
                    ),
                });
            }
            Ok(())
        }
        ExprNode::Literal(_) => Ok(()),
        ExprNode::Add(l, r)
        | ExprNode::Sub(l, r)
        | ExprNode::Mul(l, r)
        | ExprNode::Div(l, r)
        | ExprNode::And(l, r)
        | ExprNode::Or(l, r) => {
            check_target_only_predicate(l, table)?;
            check_target_only_predicate(r, table)
        }
        ExprNode::Not(e) => check_target_only_predicate(e, table),
        ExprNode::Cmp { lhs, rhs, .. } => {
            check_target_only_predicate(lhs, table)?;
            check_target_only_predicate(rhs, table)
        }
        _ => Err(DjogiError::MergeBranchInvalid {
            table,
            reason: "BY SOURCE branch condition uses an unsupported expression shape".to_string(),
        }),
    }
}

fn check_source_only_predicate(node: &ExprNode, table: &'static str) -> Result<(), DjogiError> {
    match node {
        ExprNode::Field { column } => Err(DjogiError::MergeBranchInvalid {
            table,
            reason: format!(
                "NOT MATCHED branch condition cannot reference target field `{}`",
                column
            ),
        }),
        ExprNode::OuterRefColumn {
            table: alias,
            column,
        } => {
            if *alias != SRC_ALIAS {
                return Err(DjogiError::MergeBranchInvalid {
                    table,
                    reason: format!(
                        "NOT MATCHED branch condition cannot reference target field `{}`",
                        column
                    ),
                });
            }
            Ok(())
        }
        ExprNode::Literal(_) => Ok(()),
        ExprNode::Add(l, r)
        | ExprNode::Sub(l, r)
        | ExprNode::Mul(l, r)
        | ExprNode::Div(l, r)
        | ExprNode::And(l, r)
        | ExprNode::Or(l, r) => {
            check_source_only_predicate(l, table)?;
            check_source_only_predicate(r, table)
        }
        ExprNode::Not(e) => check_source_only_predicate(e, table),
        ExprNode::Cmp { lhs, rhs, .. } => {
            check_source_only_predicate(lhs, table)?;
            check_source_only_predicate(rhs, table)
        }
        _ => Err(DjogiError::MergeBranchInvalid {
            table,
            reason: "NOT MATCHED branch condition uses an unsupported expression shape".to_string(),
        }),
    }
}

pub struct MergeBranch<S: Model, T: Model> {
    pub(crate) match_kind: MergeMatchKind,
    pub(crate) condition: Option<MergeWhenCondition<S, T>>,
    pub(crate) action: MergeAction<S, T>,
}

impl<S: Model, T: Model> Clone for MergeBranch<S, T> {
    fn clone(&self) -> Self {
        Self {
            match_kind: self.match_kind,
            condition: self.condition.clone(),
            action: self.action.clone(),
        }
    }
}

impl<S: Model, T: Model> std::fmt::Debug for MergeBranch<S, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MergeBranch")
            .field("match_kind", &self.match_kind)
            .field("condition", &self.condition)
            .field("action", &self.action)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMatchKind {
    Matched,
    NotMatchedByTarget,
    NotMatchedBySource,
}

pub enum MergeAction<S: Model, T: Model> {
    Update(Vec<MergeUpdateAssignment<S, T>>),
    Delete,
    Insert(Vec<MergeInsertColumn<S, T>>),
    /// Marker to satisfy unused type parameters.
    _Marker(PhantomData<(S, T)>),
}

impl<S: Model, T: Model> Clone for MergeAction<S, T> {
    fn clone(&self) -> Self {
        match self {
            Self::Update(v) => Self::Update(v.clone()),
            Self::Delete => Self::Delete,
            Self::Insert(v) => Self::Insert(v.clone()),
            Self::_Marker(_) => Self::_Marker(PhantomData),
        }
    }
}

impl<S: Model, T: Model> std::fmt::Debug for MergeAction<S, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Update(v) => f.debug_tuple("Update").field(v).finish(),
            Self::Delete => f.write_str("Delete"),
            Self::Insert(v) => f.debug_tuple("Insert").field(v).finish(),
            Self::_Marker(_) => f.write_str("_Marker"),
        }
    }
}

pub struct MergeOnEq<S: Model, T: Model> {
    pub(crate) target_col: &'static str,
    pub(crate) source_col: &'static str,
    pub(crate) _marker: PhantomData<(S, T)>,
}

impl<S: Model, T: Model> Clone for MergeOnEq<S, T> {
    fn clone(&self) -> Self {
        Self {
            target_col: self.target_col,
            source_col: self.source_col,
            _marker: PhantomData,
        }
    }
}

impl<S: Model, T: Model> std::fmt::Debug for MergeOnEq<S, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MergeOnEq")
            .field("target_col", &self.target_col)
            .field("source_col", &self.source_col)
            .finish()
    }
}

pub struct MergeWhenCondition<S: Model, T: Model> {
    pub(crate) node: ExprNode,
    pub(crate) _marker: PhantomData<(S, T)>,
}

impl<S: Model, T: Model> Clone for MergeWhenCondition<S, T> {
    fn clone(&self) -> Self {
        Self {
            node: self.node.clone(),
            _marker: PhantomData,
        }
    }
}

impl<S: Model, T: Model> std::fmt::Debug for MergeWhenCondition<S, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MergeWhenCondition")
            .field("node", &self.node)
            .finish()
    }
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

pub struct MergeUpdateAssignment<S: Model, T: Model> {
    pub(crate) target_col: &'static str,
    pub(crate) value: MergeValue<S, T>,
    pub(crate) _marker: PhantomData<(S, T)>,
}

impl<S: Model, T: Model> Clone for MergeUpdateAssignment<S, T> {
    fn clone(&self) -> Self {
        Self {
            target_col: self.target_col,
            value: self.value.clone(),
            _marker: PhantomData,
        }
    }
}

impl<S: Model, T: Model> std::fmt::Debug for MergeUpdateAssignment<S, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MergeUpdateAssignment")
            .field("target_col", &self.target_col)
            .field("value", &self.value)
            .finish()
    }
}

pub struct MergeInsertColumn<S: Model, T: Model> {
    pub(crate) target_col: &'static str,
    pub(crate) value: MergeValue<S, T>,
    pub(crate) _marker: PhantomData<(S, T)>,
}

impl<S: Model, T: Model> Clone for MergeInsertColumn<S, T> {
    fn clone(&self) -> Self {
        Self {
            target_col: self.target_col,
            value: self.value.clone(),
            _marker: PhantomData,
        }
    }
}

impl<S: Model, T: Model> std::fmt::Debug for MergeInsertColumn<S, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MergeInsertColumn")
            .field("target_col", &self.target_col)
            .field("value", &self.value)
            .finish()
    }
}

pub struct MergeTargetExpr<T: Model, V> {
    pub(crate) node: ExprNode,
    pub(crate) _marker: PhantomData<(T, V)>,
}

impl<T: Model, V> Clone for MergeTargetExpr<T, V> {
    fn clone(&self) -> Self {
        Self {
            node: self.node.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T: Model, V> std::fmt::Debug for MergeTargetExpr<T, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MergeTargetExpr")
            .field("node", &self.node)
            .finish()
    }
}

pub trait IntoMergeTargetExpr<T: Model, V> {
    fn into_merge_target_expr(self) -> MergeTargetExpr<T, V>;
}

impl<T: Model, V> IntoMergeTargetExpr<T, V> for MergeTargetExpr<T, V> {
    fn into_merge_target_expr(self) -> MergeTargetExpr<T, V> {
        self
    }
}

impl<T: Model, V> IntoMergeTargetExpr<T, V> for FieldRef<T, V> {
    fn into_merge_target_expr(self) -> MergeTargetExpr<T, V> {
        MergeTargetExpr {
            node: ExprNode::Field {
                column: self.column(),
            },
            _marker: PhantomData,
        }
    }
}

impl<T: Model, V> IntoMergeTargetExpr<T, V> for DjogiField<T, V> {
    fn into_merge_target_expr(self) -> MergeTargetExpr<T, V> {
        self.sql.into_merge_target_expr()
    }
}

pub(crate) enum MergeValue<S: Model, T: Model> {
    Literal(crate::query::condition::FilterValue, PhantomData<(S, T)>),
    SourceField(&'static str, PhantomData<(S, T)>),
    TargetExpr(crate::expr::node::ExprNode, PhantomData<(S, T)>),
}

impl<S: Model, T: Model> Clone for MergeValue<S, T> {
    fn clone(&self) -> Self {
        match self {
            Self::Literal(v, _) => Self::Literal(v.clone(), PhantomData),
            Self::SourceField(c, _) => Self::SourceField(c, PhantomData),
            Self::TargetExpr(e, _) => Self::TargetExpr(e.clone(), PhantomData),
        }
    }
}

impl<S: Model, T: Model> std::fmt::Debug for MergeValue<S, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Literal(v, _) => f.debug_tuple("Literal").field(v).finish(),
            Self::SourceField(c, _) => f.debug_tuple("SourceField").field(c).finish(),
            Self::TargetExpr(e, _) => f.debug_tuple("TargetExpr").field(e).finish(),
        }
    }
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
        E: IntoMergeTargetExpr<T, V>,
    {
        let expr = value.into_merge_target_expr();
        MergeUpdateAssignment {
            target_col: self.column(),
            value: MergeValue::TargetExpr(expr.node, PhantomData),
            _marker: PhantomData,
        }
    }

    /// Bind this target column to an expression for a `MERGE` insert action.
    pub fn merge_insert_expr<S: Model, E>(self, value: E) -> MergeInsertColumn<S, T>
    where
        E: IntoMergeTargetExpr<T, V>,
    {
        let expr = value.into_merge_target_expr();
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
                    table: SRC_ALIAS,
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
        E: IntoMergeTargetExpr<T, V>,
    {
        self.sql.merge_set_expr(value)
    }

    pub fn merge_insert_expr<S: Model, E>(self, value: E) -> MergeInsertColumn<S, T>
    where
        E: IntoMergeTargetExpr<T, V>,
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
    fn merge_update_changed_without_source_mapping_is_guarded() {
        let source: QuerySet<Fake> = QuerySet::new();
        let stmt = source
            .merge_into::<Fake, _, _>(|tgt, src| tgt.id().merge_on_eq(src.id()))
            .when_matched_update_changed(vec![Fake::fields().payload().merge_set("x".to_string())]);

        let branch = stmt
            .branches
            .first()
            .expect("expected one WHEN MATCHED UPDATE branch");
        assert!(
            branch.condition.is_some(),
            "update_changed must not degrade to unconditional MATCHED UPDATE"
        );
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

    #[test]
    fn merge_validation_rejects_structural_none_with_by_source() {
        let source: QuerySet<Fake> = QuerySet::new().none();
        let stmt = source
            .merge_into::<Fake, _, _>(|tgt, src| tgt.id().merge_on_eq(src.id()))
            .when_not_matched_by_source_then_delete(None::<MergeWhenCondition<Fake, Fake>>);

        let res = stmt.validate();
        assert!(matches!(res, Err(DjogiError::MergeSourceInvalid { .. })));
        if let Err(DjogiError::MergeSourceInvalid { reason, .. }) = res {
            assert!(reason.contains("structural-empty source (.none()) is rejected"));
        }
    }

    #[test]
    fn merge_validation_rejects_by_source_with_source_ref() {
        let source: QuerySet<Fake> = QuerySet::new();
        let stmt = source
            .merge_into::<Fake, _, _>(|tgt, src| tgt.id().merge_on_eq(src.id()))
            .when_not_matched_by_source_then_delete(Some(
                Fake::fields()
                    .payload()
                    .is_distinct_from_source(Fake::fields().payload()),
            ));

        let res = stmt.validate();
        assert!(matches!(res, Err(DjogiError::MergeBranchInvalid { .. })));
        if let Err(DjogiError::MergeBranchInvalid { reason, .. }) = res {
            assert!(reason.contains("BY SOURCE branch condition cannot reference source field"));
        }
    }

    #[test]
    fn merge_validation_rejects_by_source_update_copy_from_source() {
        let source: QuerySet<Fake> = QuerySet::new();
        let stmt = source
            .merge_into::<Fake, _, _>(|tgt, src| tgt.id().merge_on_eq(src.id()))
            .when_not_matched_by_source_then_update(
                None::<MergeWhenCondition<Fake, Fake>>,
                Fake::fields()
                    .payload()
                    .merge_copy_from(Fake::fields().payload()),
            );

        let res = stmt.validate();
        assert!(matches!(res, Err(DjogiError::MergeBranchInvalid { .. })));
        if let Err(DjogiError::MergeBranchInvalid { reason, .. }) = res {
            assert!(reason.contains("BY SOURCE branch update cannot reference source field"));
        }
    }

    #[test]
    fn merge_validation_rejects_not_matched_target_condition_with_target_ref() {
        let source: QuerySet<Fake> = QuerySet::new();
        let stmt = source
            .merge_into::<Fake, _, _>(|tgt, src| tgt.id().merge_on_eq(src.id()))
            .when_not_matched_then_insert(
                Some(
                    Fake::fields()
                        .payload()
                        .is_distinct_from_source(Fake::fields().payload()),
                ),
                Fake::fields()
                    .payload()
                    .merge_insert_from(Fake::fields().payload()),
            );

        let res = stmt.validate();
        assert!(matches!(res, Err(DjogiError::MergeBranchInvalid { .. })));
        if let Err(DjogiError::MergeBranchInvalid { reason, .. }) = res {
            assert!(reason.contains("NOT MATCHED branch condition cannot reference target field"));
        }
    }

    #[test]
    fn merge_boolean_predicates_preserve_tree_grouping() {
        let source: QuerySet<Fake> = QuerySet::new();
        let condition = Fake::fields()
            .id()
            .is_distinct_from_source(Fake::fields().id())
            .and(
                Fake::fields()
                    .payload()
                    .is_distinct_from_source(Fake::fields().payload())
                    .or(Fake::fields()
                        .id()
                        .is_distinct_from_source(Fake::fields().id())),
            );
        let stmt = source
            .merge_into::<Fake, _, _>(|tgt, src| tgt.id().merge_on_eq(src.id()))
            .when_matched_and_delete(Some(condition));

        let acc = build_merge(&stmt.source, &stmt.on, &stmt.branches, None).unwrap();
        let sql = acc.sql();

        assert!(
            sql.contains(
                "WHEN MATCHED AND tgt.id IS DISTINCT FROM __djogi_src.id AND (tgt.payload IS DISTINCT FROM __djogi_src.payload OR tgt.id IS DISTINCT FROM __djogi_src.id) THEN DELETE"
            ),
            "SQL did not preserve nested OR grouping: {sql}"
        );
    }
}
