#![allow(clippy::manual_async_fn)]

use crate::DjogiError;
use crate::context::DjogiContext;
use crate::model::Model;
use crate::pg::accumulator::{SqlAccumulator, as_params};
use crate::pg::decode::FromPgRow;
use crate::query::set_op::SetOpKind;
use crate::query::queryset::QuerySet;
use crate::query::visage_queryset::VisageQuerySet;
use crate::visage::DjogiVisage;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;

pub(crate) trait CrossArm<R: FromPgRow>: Send + Sync {
    fn emit(&self, acc: &mut SqlAccumulator, side: &'static str) -> Result<(), DjogiError>;
    fn intended_tenant(&self, ctx: &DjogiContext) -> (&'static str, Option<String>);
    fn fire_tenant<'a>(
        &'a self,
        ctx: &'a mut DjogiContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), DjogiError>> + Send + 'a>>;
}

struct QuerySetArm<M, R>
where
    M: Model + FromPgRow + Send + Unpin + 'static,
    R: FromPgRow,
{
    qs: QuerySet<M>,
    _row: PhantomData<fn() -> R>,
}

impl<M, R> CrossArm<R> for QuerySetArm<M, R>
where
    M: Model + FromPgRow + Send + Unpin + 'static,
    R: FromPgRow,
{
    fn emit(&self, acc: &mut SqlAccumulator, side: &'static str) -> Result<(), DjogiError> {
        acc.push_sql("(");
        crate::query::set_op::validate_arm::<M>(&self.qs, side)?;
        if self.qs.is_empty() {
            acc.push_sql("SELECT ");
            acc.push_sql(<M as FromPgRow>::COLUMN_LIST);
            acc.push_sql(" FROM ");
            acc.push_sql(M::table_name());
            acc.push_sql(" WHERE FALSE");
        } else {
            let inner = crate::query::sql::build_select(&self.qs)?;
            acc.extend_with(inner);
        }
        acc.push_sql(")");
        Ok(())
    }

    fn intended_tenant(&self, ctx: &DjogiContext) -> (&'static str, Option<String>) {
        if M::descriptor().tenant_key.is_none() {
            return (std::any::type_name::<M>(), None);
        }
        let intended = ctx.auth().and_then(|a| a.tenant_id.clone());
        (std::any::type_name::<M>(), intended)
    }

    fn fire_tenant<'a>(
        &'a self,
        ctx: &'a mut DjogiContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), DjogiError>> + Send + 'a>> {
        Box::pin(async move { crate::query::terminal::auto_set_tenant::<M>(ctx).await })
    }
}

struct VisageArm<V, R>
where
    V: DjogiVisage + FromPgRow + Send + Unpin + 'static,
    R: FromPgRow,
{
    qs: VisageQuerySet<V>,
    _row: PhantomData<fn() -> R>,
}

impl<V, R> CrossArm<R> for VisageArm<V, R>
where
    V: DjogiVisage + FromPgRow + Send + Unpin + 'static,
    R: FromPgRow,
{
    fn emit(&self, acc: &mut SqlAccumulator, _side: &'static str) -> Result<(), DjogiError> {
        acc.push_sql("(");
        let inner = crate::query::visage_queryset::build_visage_select(&self.qs)
            .map_err(DjogiError::from)?;
        acc.extend_with(inner);
        acc.push_sql(")");
        Ok(())
    }

    fn intended_tenant(&self, ctx: &DjogiContext) -> (&'static str, Option<String>) {
        if <V::Model as Model>::descriptor().tenant_key.is_none() {
            return (std::any::type_name::<V::Model>(), None);
        }
        let intended = ctx.auth().and_then(|a| a.tenant_id.clone());
        (std::any::type_name::<V::Model>(), intended)
    }

    fn fire_tenant<'a>(
        &'a self,
        ctx: &'a mut DjogiContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), DjogiError>> + Send + 'a>> {
        Box::pin(async move { crate::query::terminal::auto_set_tenant::<V::Model>(ctx).await })
    }
}

mod sealed {
    pub trait Sealed {}
    impl<M> Sealed for super::QuerySet<M> where
        M: super::Model + super::FromPgRow + Send + Unpin + 'static { }
    impl<V> Sealed for super::VisageQuerySet<V> where
        V: super::DjogiVisage + super::FromPgRow + Send + Unpin + 'static { }
}

pub trait IntoCrossArm<R: FromPgRow>: sealed::Sealed {
    #[doc(hidden)]
    fn into_cross_arm(self) -> Box<dyn CrossArm<R>>;
}

impl<M, R> IntoCrossArm<R> for QuerySet<M>
where
    M: Model + FromPgRow + Send + Unpin + 'static,
    R: FromPgRow + 'static,
{
    fn into_cross_arm(self) -> Box<dyn CrossArm<R>> {
        Box::new(QuerySetArm::<M, R> { qs: self, _row: PhantomData })
    }
}

impl<V, R> IntoCrossArm<R> for VisageQuerySet<V>
where
    V: DjogiVisage + FromPgRow + Send + Unpin + 'static,
    R: FromPgRow + 'static,
{
    fn into_cross_arm(self) -> Box<dyn CrossArm<R>> {
        Box::new(VisageArm::<V, R> { qs: self, _row: PhantomData })
    }
}

pub struct CrossModelSetOpQuerySet<R: FromPgRow> {
    pub(crate) left: Box<dyn CrossArm<R>>,
    pub(crate) op: SetOpKind,
    pub(crate) right: Box<dyn CrossArm<R>>,
    pub(crate) ordering: Vec<OuterColumnOrder>,
    pub(crate) limit: Option<i64>,
    pub(crate) offset: Option<i64>,
    _row: PhantomData<fn() -> R>,
}

impl<R: FromPgRow> std::fmt::Debug for CrossModelSetOpQuerySet<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrossModelSetOpQuerySet")
            .field("op", &self.op)
            .field("ordering", &self.ordering)
            .field("limit", &self.limit)
            .field("offset", &self.offset)
            .finish_non_exhaustive()
    }
}

#[must_use = "cross-model set ops are lazy"]
pub fn union_as<R, A, B>(left: A, right: B) -> CrossModelSetOpQuerySet<R>
where R: FromPgRow, A: IntoCrossArm<R>, B: IntoCrossArm<R> {
    CrossModelSetOpQuerySet { left: left.into_cross_arm(), op: SetOpKind::Union, right: right.into_cross_arm(), ordering: Vec::new(), limit: None, offset: None, _row: PhantomData }
}

#[must_use = "cross-model set ops are lazy"]
pub fn union_all_as<R, A, B>(left: A, right: B) -> CrossModelSetOpQuerySet<R>
where R: FromPgRow, A: IntoCrossArm<R>, B: IntoCrossArm<R> {
    CrossModelSetOpQuerySet { left: left.into_cross_arm(), op: SetOpKind::UnionAll, right: right.into_cross_arm(), ordering: Vec::new(), limit: None, offset: None, _row: PhantomData }
}

#[must_use = "cross-model set ops are lazy"]
pub fn intersect_as<R, A, B>(left: A, right: B) -> CrossModelSetOpQuerySet<R>
where R: FromPgRow, A: IntoCrossArm<R>, B: IntoCrossArm<R> {
    CrossModelSetOpQuerySet { left: left.into_cross_arm(), op: SetOpKind::Intersect, right: right.into_cross_arm(), ordering: Vec::new(), limit: None, offset: None, _row: PhantomData }
}

#[must_use = "cross-model set ops are lazy"]
pub fn except_as<R, A, B>(left: A, right: B) -> CrossModelSetOpQuerySet<R>
where R: FromPgRow, A: IntoCrossArm<R>, B: IntoCrossArm<R> {
    CrossModelSetOpQuerySet { left: left.into_cross_arm(), op: SetOpKind::Except, right: right.into_cross_arm(), ordering: Vec::new(), limit: None, offset: None, _row: PhantomData }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OuterOrder { Asc, Desc }

impl OuterOrder {
    fn keyword(self) -> &'static str {
        match self { OuterOrder::Asc => "ASC", OuterOrder::Desc => "DESC" }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct OuterColumnOrder { column: String, direction: OuterOrder }

impl OuterColumnOrder {
    fn emit(&self, acc: &mut SqlAccumulator) {
        acc.push_sql(&self.column);
        acc.push_sql(" ");
        acc.push_sql(self.direction.keyword());
    }
    fn validate(&self) -> Result<(), DjogiError> {
        if self.column.starts_with("__djogi_") {
            return Err(DjogiError::SetOpOuterOrderingInvalid { table: "cross-model set op", reason: "outer ORDER BY column names the framework-reserved `__djogi_` namespace" });
        }
        let bytes = self.column.as_bytes();
        let ok = !bytes.is_empty() && bytes.len() <= 63
            && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
            && bytes[1..].iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_');
        if ok { Ok(()) } else {
            Err(DjogiError::SetOpOuterOrderingInvalid { table: "cross-model set op", reason: "outer ORDER BY column must be an ASCII identifier" })
        }
    }
}

impl<R: FromPgRow> CrossModelSetOpQuerySet<R> {
    pub fn op(&self) -> SetOpKind { self.op }

    #[must_use = "cross-model set ops are lazy"]
    pub fn order_by(mut self, column: impl Into<String>, direction: OuterOrder) -> Self {
        self.ordering.push(OuterColumnOrder { column: column.into(), direction });
        self
    }

    #[must_use = "cross-model set ops are lazy"]
    pub fn limit(mut self, n: u64) -> Self {
        debug_assert!(n <= i64::MAX as u64);
        self.limit = Some(n as i64);
        self
    }

    #[must_use = "cross-model set ops are lazy"]
    pub fn offset(mut self, n: u64) -> Self {
        debug_assert!(n <= i64::MAX as u64);
        self.offset = Some(n as i64);
        self
    }
}

fn build_cross_set_op_select_inner<R: FromPgRow>(
    acc: &mut SqlAccumulator,
    sop: &CrossModelSetOpQuerySet<R>,
) -> Result<(), DjogiError> {
    for o in &sop.ordering { o.validate()?; }
    sop.left.emit(acc, "left")?;
    acc.push_sql(" ");
    acc.push_sql(sop.op.keyword());
    acc.push_sql(" ");
    sop.right.emit(acc, "right")?;
    if !sop.ordering.is_empty() {
        acc.push_sql(" ORDER BY ");
        for (i, o) in sop.ordering.iter().enumerate() {
            if i > 0 { acc.push_sql(", "); }
            o.emit(acc);
        }
    }
    if let Some(n) = sop.limit {
        acc.push_sql(" LIMIT ");
        acc.push_bind(n);
    }
    if let Some(n) = sop.offset {
        acc.push_sql(" OFFSET ");
        acc.push_bind(n);
    }
    Ok(())
}

pub(crate) fn build_cross_set_op_select<R: FromPgRow>(
    sop: &CrossModelSetOpQuerySet<R>,
) -> Result<SqlAccumulator, DjogiError> {
    let mut acc = SqlAccumulator::new("");
    build_cross_set_op_select_inner(&mut acc, sop)?;
    Ok(acc)
}

pub(crate) fn build_cross_set_op_count<R: FromPgRow>(
    sop: &CrossModelSetOpQuerySet<R>,
) -> Result<SqlAccumulator, DjogiError> {
    for o in &sop.ordering { o.validate()?; }
    let mut acc = SqlAccumulator::new("SELECT COUNT(*) FROM (");
    sop.left.emit(&mut acc, "left")?;
    acc.push_sql(" ");
    acc.push_sql(sop.op.keyword());
    acc.push_sql(" ");
    sop.right.emit(&mut acc, "right")?;
    acc.push_sql(") AS sub");
    Ok(acc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[crate::model(table = "x462_left_widgets", pk = crate::HeerId)]
    #[derive(Debug, Clone)]
    pub struct LeftWidget { name: String }

    #[crate::model(table = "x462_right_gadgets", pk = crate::HeerId)]
    #[derive(Debug, Clone)]
    pub struct RightGadget { name: String }

    #[crate::model(table = "x462_combined_rows", pk = crate::HeerId)]
    #[derive(Debug, Clone)]
    pub struct CombinedRow { name: String }

    #[crate::model(table = "x462_tenant_widgets", pk = crate::HeerId, tenant_key = "org_id")]
    #[derive(Debug, Clone)]
    pub struct TenantWidget { org_id: String, name: String }

    #[test]
    fn cross_union_emits_parenthesised_arms_with_keyword_and_renumbered_binds() {
        let left = LeftWidget::objects().filter(|f| f.name().eq("a".to_string()));
        let right = RightGadget::objects().filter(|f| f.name().eq("b".to_string()));
        let sop = union_as::<CombinedRow, _, _>(left, right);

        let acc = build_cross_set_op_select(&sop).unwrap();
        let sql = acc.sql();
        assert!(sql.contains("UNION"), "{sql}");
        assert!(sql.contains("$1"), "left arm bind: {sql}");
        assert!(sql.contains("$2"), "right arm bind renumbered: {sql}");
        assert!(sql.contains("x462_left_widgets"), "{sql}");
        assert!(sql.contains("x462_right_gadgets"), "{sql}");
    }

    #[test]
    fn cross_count_wraps_in_subquery_and_strips_outer_modifiers() {
        let left = LeftWidget::objects();
        let right = RightGadget::objects();
        let sop = union_as::<CombinedRow, _, _>(left, right)
            .limit(5)
            .offset(2);

        let acc = build_cross_set_op_count(&sop).unwrap();
        let sql = acc.sql();
        assert!(sql.starts_with("SELECT COUNT(*) FROM ("), "{sql}");
        assert!(sql.trim_end().ends_with(") AS sub"), "{sql}");
        assert!(!sql.contains("LIMIT"), "count must strip outer LIMIT: {sql}");
        assert!(!sql.contains("OFFSET"), "count must strip outer OFFSET: {sql}");
    }

    #[test]
    fn cross_outer_order_by_limit_offset_emit_after_arms() {
        let left = LeftWidget::objects();
        let right = RightGadget::objects();
        let sop = union_as::<CombinedRow, _, _>(left, right)
            .order_by("name", OuterOrder::Asc)
            .order_by("created_at", OuterOrder::Desc)
            .limit(10)
            .offset(3);

        let acc = build_cross_set_op_select(&sop).unwrap();
        let sql = acc.sql();
        let order_idx = sql.find("ORDER BY").expect("order by present");
        let union_idx = sql.find("UNION").expect("union present");
        assert!(order_idx > union_idx, "ORDER BY must follow the operator: {sql}");
        assert!(sql.contains("ORDER BY name ASC, created_at DESC"), "{sql}");
        assert!(sql.contains("LIMIT"), "{sql}");
        assert!(sql.contains("OFFSET"), "{sql}");
    }

    #[test]
    fn cross_order_by_rejects_non_identifier_column() {
        let sop = union_as::<CombinedRow, _, _>(LeftWidget::objects(), RightGadget::objects())
            .order_by("name; DROP TABLE x", OuterOrder::Asc);
        let err = match build_cross_set_op_select(&sop) {
            Ok(_) => panic!("expected error for non-identifier column"),
            Err(e) => e,
        };
        assert!(
            matches!(err, DjogiError::SetOpOuterOrderingInvalid { .. }),
            "non-identifier outer order column must be rejected: {err:?}"
        );
    }

    #[test]
    fn cross_order_by_rejects_framework_reserved_column() {
        let sop = union_as::<CombinedRow, _, _>(LeftWidget::objects(), RightGadget::objects())
            .order_by("__djogi_tenant_id", OuterOrder::Asc);
        let err = match build_cross_set_op_select(&sop) {
            Ok(_) => panic!("expected error for reserved column"),
            Err(e) => e,
        };
        assert!(
            matches!(err, DjogiError::SetOpOuterOrderingInvalid { reason, .. }
                if reason.contains("__djogi_")),
            "framework-reserved outer order column must be rejected: {err:?}"
        );
    }
}
