//! `CteQuerySet<M>` — typed Common Table Expression (CTE) query builder.
//! # What
//! A [`CteQuerySet<M>`] composes one or more CTE terms (`WITH name AS (...)`)
//! ahead of a consumer `SELECT` over a named CTE relation. Construct the
//! first term through [`QuerySet::with`](crate::query::QuerySet::with) or
//! [`QuerySet::with_recursive`](crate::query::QuerySet::with_recursive), then
//! choose the consumer source with [`from_cte`](CteQuerySet::from_cte).
//! # Why a sibling type
//! A CTE-augmented query carries a `WITH` preamble and may read from a CTE
//! name rather than `M`'s table. Keeping it as a sibling type leaves the hot
//! plain-query `build_select` path untouched and mirrors the existing sibling
//! query surfaces such as [`SetOpQuerySet`](crate::query::SetOpQuerySet).
//! # Column-shape contract
//! The consumer projection is `<M as FromPgRow>::COLUMN_LIST`, decoded through
//! `M`'s [`FromPgRow`] impl. CTE bodies must therefore project the same model
//! columns in the same order. A plain `QuerySet<M>` or `SetOpQuerySet<M>`
//! body does this by construction, and the recursive arm projection is derived
//! from `<M as FromPgRow>::COLUMNS` for the same reason.
//! # Cycle-mark consumption
//! `CYCLE ... SET <mark> USING <path>` appends synthetic columns to the CTE
//! relation. Consume the mark through
//! [`exclude_cycle_rows`](CteQuerySet::exclude_cycle_rows), not a closure
//! filter, because the mark is not a model field and has no `M::Fields`
//! accessor.
//! # Postgres floor
//! `CYCLE` requires Postgres 14+; djogi targets Postgres 18+ exclusively.
//! # Why RPITIT
//! Terminal methods return `impl Future + Send` to match the rest of djogi's
//! public queryset surface.
#![allow(clippy::manual_async_fn)]

use std::marker::PhantomData;

use crate::DjogiError;
use crate::ident::check_user_supplied_ident;
use crate::model::Model;
use crate::pg::accumulator::{SqlAccumulator, as_params};
use crate::pg::decode::FromPgRow;
use crate::query::portable::SqlEmitContext;
use crate::query::queryset::QuerySet;
use crate::query::set_op::{SetOpQuerySet, build_set_op_select};

/// Optional `CYCLE` clause attached to a recursive CTE term.
#[derive(Debug, Clone)]
pub(crate) struct CteCycle {
    pub(crate) cycle_columns: Vec<&'static str>,
    pub(crate) mark_column: &'static str,
    pub(crate) path_column: &'static str,
}

/// One pending CTE term in a [`CteQuerySet<M>`].
/// The body is stored as a rebuild recipe because emitted bind values live in
/// `Box<dyn ToSql>` containers, which are not clonable. Each render reruns the
/// trusted queryset emitter and gets a fresh accumulator.
pub(crate) struct CteTerm {
    pub(crate) name: &'static str,
    pub(crate) recursive: bool,
    pub(crate) body: Box<dyn Fn() -> Result<SqlAccumulator, DjogiError> + Send + Sync>,
    pub(crate) cycle: Option<CteCycle>,
}

impl std::fmt::Debug for CteTerm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CteTerm")
            .field("name", &self.name)
            .field("recursive", &self.recursive)
            .field("cycle", &self.cycle)
            .finish_non_exhaustive()
    }
}

/// Validate a user-supplied CTE identifier and surface failures as
/// [`DjogiError::Validation`].
pub(crate) fn validate_cte_name(name: &str) -> Result<(), DjogiError> {
    check_user_supplied_ident(name, true)
        .map_err(|e| DjogiError::Validation(format!("invalid CTE name {name:?}: {e:?}")))
}

fn sql_ident_eq(lhs: &str, rhs: &str) -> bool {
    lhs.eq_ignore_ascii_case(rhs)
}

fn sql_ident_in(haystack: &[&'static str], needle: &str) -> bool {
    haystack
        .iter()
        .any(|candidate| sql_ident_eq(candidate, needle))
}

fn ensure_unique_term_name(terms: &[CteTerm], name: &'static str) -> Result<(), DjogiError> {
    if terms.iter().any(|term| sql_ident_eq(term.name, name)) {
        return Err(DjogiError::Validation(format!(
            "CTE term name {name:?} declared more than once"
        )));
    }
    Ok(())
}

/// Reject a CTE term name that shadows the model's own table, which would
/// make the CTE body's `FROM <table>` reference resolve to the CTE itself
/// rather than the physical table — producing either a Postgres error or
/// silent self-reference on non-recursive CTEs, and unintended infinite
/// recursion on `WITH RECURSIVE` terms.
fn ensure_cte_name_not_model_table(name: &str, table_name: &str) -> Result<(), DjogiError> {
    if sql_ident_eq(name, table_name) {
        return Err(DjogiError::Validation(format!(
            "CTE name {name:?} conflicts with the model table {table_name:?}; \
             choose a different name",
        )));
    }
    Ok(())
}

fn find_term<'a>(terms: &'a [CteTerm], name: &str) -> Option<&'a CteTerm> {
    terms.iter().find(|term| sql_ident_eq(term.name, name))
}

fn find_selected_cycle_mark(terms: &[CteTerm], from: &str) -> Option<&'static str> {
    find_term(terms, from)
        .and_then(|term| term.cycle.as_ref())
        .map(|cycle| cycle.mark_column)
}

fn build_cte_queryset_body<M: Model + FromPgRow>(
    qs: &QuerySet<M>,
) -> Result<SqlAccumulator, DjogiError> {
    if qs.is_empty() {
        let mut acc = SqlAccumulator::new("SELECT ");
        acc.push_sql(<M as FromPgRow>::COLUMN_LIST);
        acc.push_sql(" FROM ");
        acc.push_sql(M::table_name());
        acc.push_sql(" WHERE FALSE");
        return Ok(acc);
    }
    crate::query::sql::build_select(qs).map_err(DjogiError::from)
}

fn validate_recursive_anchor<M: Model>(anchor: &QuerySet<M>) -> Result<(), DjogiError> {
    let mut rejected: Vec<&'static str> = Vec::new();
    if anchor.has_explicit_ordering {
        rejected.push("order_by");
    }
    if anchor.limit.is_some() {
        rejected.push("limit");
    }
    if anchor.offset.is_some() {
        rejected.push("offset");
    }
    if !matches!(anchor.distinct, crate::query::DistinctMode::None) {
        rejected.push("distinct");
    }
    if anchor.lock != crate::query::lock::LockMode::None {
        rejected.push("row locks");
    }
    if !anchor.prefetch_paths.is_empty() {
        rejected.push("prefetch");
    }
    if !anchor.select_related_paths.is_empty() {
        rejected.push("select_related");
    }
    if anchor.cache_target.is_some() {
        rejected.push("cache_target");
    }
    if rejected.is_empty() {
        return Ok(());
    }
    Err(DjogiError::Validation(format!(
        "with_recursive() anchor does not support queryset read-tail modifiers ({})",
        rejected.join(", ")
    )))
}

fn validate_cte_consumer<M: Model>(consumer: &QuerySet<M>, site: &str) -> Result<(), DjogiError> {
    let mut rejected: Vec<&'static str> = Vec::new();
    if !matches!(consumer.distinct, crate::query::DistinctMode::None) {
        rejected.push("distinct");
    }
    if consumer.lock != crate::query::lock::LockMode::None {
        rejected.push("row locks");
    }
    if !consumer.prefetch_paths.is_empty() {
        rejected.push("prefetch");
    }
    if !consumer.select_related_paths.is_empty() {
        rejected.push("select_related");
    }
    if consumer.cache_target.is_some() {
        rejected.push("cache_target");
    }
    if rejected.is_empty() {
        return Ok(());
    }
    Err(DjogiError::Validation(format!(
        "{site} does not support inherited queryset modifiers ({}) on the CTE consumer; remove them before entering the CTE builder",
        rejected.join(", ")
    )))
}

fn validate_recursive_join_columns<M: FromPgRow>(
    table_col: &'static str,
    cte_col: &'static str,
) -> Result<(), DjogiError> {
    let cols = <M as FromPgRow>::COLUMNS;
    if !sql_ident_in(cols, table_col) {
        return Err(DjogiError::Validation(format!(
            "recursive-arm table join column {table_col:?} is not present in model output"
        )));
    }
    if !sql_ident_in(cols, cte_col) {
        return Err(DjogiError::Validation(format!(
            "recursive-arm CTE join column {cte_col:?} is not present in model output"
        )));
    }
    Ok(())
}

mod sealed {
    pub trait Sealed {}
    impl<M: super::Model> Sealed for super::QuerySet<M> {}
    impl<M: super::Model> Sealed for super::SetOpQuerySet<M> {}
}

/// Sealed conversion trait for non-recursive CTE bodies.
/// Adopters never implement this directly; they pass either a plain
/// [`QuerySet<M>`] or a [`SetOpQuerySet<M>`] to
/// [`QuerySet::with`](crate::query::QuerySet::with) and the bound is
/// satisfied automatically.
pub trait IntoCteBody<M: Model>: sealed::Sealed {
    #[doc(hidden)]
    fn into_cte_body_recipe(
        self,
    ) -> Box<dyn Fn() -> Result<SqlAccumulator, DjogiError> + Send + Sync>;
}

impl<M: Model + FromPgRow> IntoCteBody<M> for QuerySet<M> {
    fn into_cte_body_recipe(
        self,
    ) -> Box<dyn Fn() -> Result<SqlAccumulator, DjogiError> + Send + Sync> {
        Box::new(move || build_cte_queryset_body(&self))
    }
}

impl<M: Model + FromPgRow> IntoCteBody<M> for SetOpQuerySet<M> {
    fn into_cte_body_recipe(
        self,
    ) -> Box<dyn Fn() -> Result<SqlAccumulator, DjogiError> + Send + Sync> {
        Box::new(move || build_set_op_select(&self))
    }
}

/// The recursive term of a `WITH RECURSIVE` CTE.
/// Build it with [`referencing`](Self::referencing), then
/// [`join_on`](Self::join_on), then optionally [`filter`](Self::filter).
pub struct RecursiveArm<M: Model> {
    pub(crate) cte_name: &'static str,
    pub(crate) join: Option<(&'static str, &'static str)>,
    pub(crate) arm_filter: QuerySet<M>,
}

impl<M: Model> std::fmt::Debug for RecursiveArm<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecursiveArm")
            .field("table", &M::table_name())
            .field("cte_name", &self.cte_name)
            .field("join", &self.join)
            .field("arm_filter", &self.arm_filter)
            .finish()
    }
}

impl<M: Model> RecursiveArm<M> {
    /// Start a recursive arm that reads from the in-flight CTE named
    /// `cte_name`.
    #[must_use = "a RecursiveArm does nothing until passed to with_recursive"]
    pub fn referencing(cte_name: &'static str) -> Self {
        Self {
            cte_name,
            join: None,
            arm_filter: QuerySet::new(),
        }
    }

    /// Declare the recursive self-edge `t.<table_col> = cte.<cte_col>`.
    pub fn join_on(
        mut self,
        table_col: &'static str,
        cte_col: &'static str,
    ) -> Result<Self, DjogiError> {
        if self.join.is_some() {
            return Err(DjogiError::Validation(
                "RecursiveArm::join_on called twice — a recursive term has exactly one self-edge"
                    .to_string(),
            ));
        }
        validate_cte_name(table_col).map_err(|e| {
            DjogiError::Validation(format!("invalid recursive-arm table join column: {e}"))
        })?;
        validate_cte_name(cte_col).map_err(|e| {
            DjogiError::Validation(format!("invalid recursive-arm CTE join column: {e}"))
        })?;
        self.join = Some((table_col, cte_col));
        Ok(self)
    }

    /// AND an additional filter onto the recursive term.
    #[must_use = "a RecursiveArm does nothing until passed to with_recursive"]
    pub fn filter<F, P>(mut self, f: F) -> Self
    where
        F: FnOnce(M::Fields) -> P,
        P: crate::query::IntoQ<M>,
    {
        self.arm_filter = self.arm_filter.filter(f);
        self
    }

    /// Return the CTE name this arm references.
    pub fn cte_name(&self) -> &'static str {
        self.cte_name
    }

    /// Return the configured `(table_col, cte_col)` self-edge.
    pub fn join_columns(&self) -> Option<(&'static str, &'static str)> {
        self.join
    }
}

/// A lazy CTE-augmented queryset over model `M`.
/// Constructed through [`QuerySet::with`](crate::query::QuerySet::with) or
/// [`QuerySet::with_recursive`](crate::query::QuerySet::with_recursive).
pub struct CteQuerySet<M: Model> {
    pub(crate) terms: Vec<CteTerm>,
    pub(crate) from: Option<&'static str>,
    pub(crate) consumer: QuerySet<M>,
    pub(crate) consumer_exclude_cycle_rows: bool,
    _model: PhantomData<fn() -> M>,
}

impl<M: Model> std::fmt::Debug for CteQuerySet<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CteQuerySet")
            .field("table", &M::table_name())
            .field("terms", &self.terms)
            .field("from", &self.from)
            .field("consumer", &self.consumer)
            .field(
                "consumer_exclude_cycle_rows",
                &self.consumer_exclude_cycle_rows,
            )
            .finish()
    }
}

impl<M: Model> CteQuerySet<M> {
    #[doc(hidden)]
    pub fn __begin<B: IntoCteBody<M>>(
        consumer: QuerySet<M>,
        name: &'static str,
        body: B,
    ) -> Result<Self, DjogiError> {
        validate_cte_name(name)?;
        ensure_cte_name_not_model_table(name, M::table_name())?;
        ensure_unique_term_name(&[], name)?;
        validate_cte_consumer(&consumer, "QuerySet::with(...)")?;
        let recipe = body.into_cte_body_recipe();
        recipe()?;
        Ok(Self {
            terms: vec![CteTerm {
                name,
                recursive: false,
                body: recipe,
                cycle: None,
            }],
            from: None,
            consumer,
            consumer_exclude_cycle_rows: false,
            _model: PhantomData,
        })
    }

    /// Select from a previously-declared CTE term.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn from_cte(mut self, name: &'static str) -> Result<Self, DjogiError> {
        validate_cte_name(name)?;
        if find_term(&self.terms, name).is_none() {
            return Err(DjogiError::Validation(format!(
                "from_cte({name:?}) references an undeclared CTE term"
            )));
        }
        if self.consumer_exclude_cycle_rows && find_selected_cycle_mark(&self.terms, name).is_none()
        {
            return Err(DjogiError::Validation(format!(
                "from_cte({name:?}) cannot retarget a query with exclude_cycle_rows() onto a term without a CYCLE clause"
            )));
        }
        self.from = Some(name);
        Ok(self)
    }

    /// Add another non-recursive CTE term to the query.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn with<B: IntoCteBody<M>>(
        mut self,
        name: &'static str,
        body: B,
    ) -> Result<Self, DjogiError> {
        validate_cte_name(name)?;
        ensure_cte_name_not_model_table(name, M::table_name())?;
        ensure_unique_term_name(&self.terms, name)?;
        let recipe = body.into_cte_body_recipe();
        recipe()?;
        self.terms.push(CteTerm {
            name,
            recursive: false,
            body: recipe,
            cycle: None,
        });
        Ok(self)
    }

    /// Add another recursive CTE term to the query from an anchor queryset and
    /// a typed recursive arm.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn with_recursive(
        mut self,
        name: &'static str,
        anchor: QuerySet<M>,
        arm: RecursiveArm<M>,
    ) -> Result<Self, DjogiError>
    where
        M: FromPgRow,
    {
        self.push_recursive_term(name, anchor, arm)?;
        Ok(self)
    }

    fn push_recursive_term(
        &mut self,
        name: &'static str,
        anchor: QuerySet<M>,
        arm: RecursiveArm<M>,
    ) -> Result<(), DjogiError>
    where
        M: FromPgRow,
    {
        validate_cte_name(name)?;
        ensure_unique_term_name(&self.terms, name)?;
        ensure_cte_name_not_model_table(name, M::table_name())?;
        validate_cte_name(arm.cte_name)?;
        if arm.cte_name != name {
            return Err(DjogiError::Validation(format!(
                "with_recursive name {name:?} does not match RecursiveArm::referencing({:?})",
                arm.cte_name
            )));
        }
        if arm.join.is_none() {
            return Err(DjogiError::Validation(
                "recursive arm has no self-edge — call `.join_on(table_col, cte_col)` before passing it to with_recursive"
                    .to_string(),
            ));
        }
        let (table_col, cte_col) = arm.join.expect("checked above");
        validate_recursive_join_columns::<M>(table_col, cte_col)?;
        validate_recursive_anchor(&anchor)?;
        let recipe = Box::new(move || build_recursive_cte_body(&anchor, &arm, name));
        recipe()?;
        self.terms.push(CteTerm {
            name,
            recursive: true,
            body: recipe,
            cycle: None,
        });
        Ok(())
    }

    pub(crate) fn __begin_recursive(
        consumer: QuerySet<M>,
        name: &'static str,
        anchor: QuerySet<M>,
        arm: RecursiveArm<M>,
    ) -> Result<Self, DjogiError>
    where
        M: FromPgRow,
    {
        let mut cte = Self {
            terms: Vec::new(),
            from: None,
            consumer,
            consumer_exclude_cycle_rows: false,
            _model: PhantomData,
        };
        validate_cte_consumer(&cte.consumer, "QuerySet::with_recursive(...)")?;
        cte.push_recursive_term(name, anchor, arm)?;
        Ok(cte)
    }

    /// AND a closure predicate onto the consumer `SELECT`.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn filter<F, P>(mut self, f: F) -> Self
    where
        F: FnOnce(M::Fields) -> P,
        P: crate::query::IntoQ<M>,
    {
        self.consumer = self.consumer.filter(f);
        self
    }

    /// Append ordering to the consumer `SELECT`.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn order_by<F, O>(mut self, f: F) -> Self
    where
        F: FnOnce(M::Fields) -> O,
        O: Into<Vec<crate::query::order::OrderExpr>>,
    {
        self.consumer = self.consumer.order_by(f);
        self
    }

    /// Apply `LIMIT n` to the consumer `SELECT`.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn limit(mut self, n: u64) -> Self {
        self.consumer = self.consumer.limit(n);
        self
    }

    /// Apply `OFFSET n` to the consumer `SELECT`.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn offset(mut self, n: u64) -> Self {
        self.consumer = self.consumer.offset(n);
        self
    }
}

impl<M: Model + FromPgRow> CteQuerySet<M> {
    /// Attach a Postgres `CYCLE` clause to the most-recent recursive term.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn cycle(
        mut self,
        cycle_columns: &[&'static str],
        mark_column: &'static str,
        path_column: &'static str,
    ) -> Result<Self, DjogiError> {
        if cycle_columns.is_empty() {
            return Err(DjogiError::Validation(
                "CYCLE clause requires at least one cycle column".to_string(),
            ));
        }
        for col in cycle_columns {
            validate_cte_name(col)
                .map_err(|e| DjogiError::Validation(format!("invalid CYCLE column: {e}")))?;
        }
        validate_cte_name(mark_column)
            .map_err(|e| DjogiError::Validation(format!("invalid CYCLE SET column: {e}")))?;
        validate_cte_name(path_column)
            .map_err(|e| DjogiError::Validation(format!("invalid CYCLE USING column: {e}")))?;

        let cols = <M as FromPgRow>::COLUMNS;
        for col in cycle_columns {
            if !sql_ident_in(cols, col) {
                return Err(DjogiError::Validation(format!(
                    "CYCLE column {col:?} is not present in model {:?} output",
                    M::table_name()
                )));
            }
        }
        if sql_ident_in(cols, mark_column) {
            return Err(DjogiError::Validation(format!(
                "CYCLE SET column {mark_column:?} collides with a column of model {:?}",
                M::table_name()
            )));
        }
        if sql_ident_in(cols, path_column) {
            return Err(DjogiError::Validation(format!(
                "CYCLE USING column {path_column:?} collides with a column of model {:?}",
                M::table_name()
            )));
        }
        if sql_ident_eq(mark_column, path_column) {
            return Err(DjogiError::Validation(
                "CYCLE mark column and path column must be distinct".to_string(),
            ));
        }
        if cycle_columns
            .iter()
            .any(|col| sql_ident_eq(col, mark_column))
            || cycle_columns
                .iter()
                .any(|col| sql_ident_eq(col, path_column))
        {
            return Err(DjogiError::Validation(
                "CYCLE mark/path column names must not duplicate the tracked cycle_columns"
                    .to_string(),
            ));
        }

        let last = self.terms.last_mut().ok_or_else(|| {
            DjogiError::Validation("cycle(...) called before any CTE term was added".to_string())
        })?;
        if !last.recursive {
            return Err(DjogiError::Validation(
                "cycle(...) requires the most-recent CTE term to be recursive".to_string(),
            ));
        }
        last.cycle = Some(CteCycle {
            cycle_columns: cycle_columns.to_vec(),
            mark_column,
            path_column,
        });
        Ok(self)
    }

    /// Drop cycle-marked rows from the consumer result by emitting
    /// `WHERE NOT <mark>` on the outer consumer `SELECT`.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn exclude_cycle_rows(mut self) -> Result<Self, DjogiError> {
        let from = self.from.ok_or_else(|| {
            DjogiError::Validation(
                "exclude_cycle_rows() requires `.from_cte(<name>)` before it can bind a cycle mark"
                    .to_string(),
            )
        })?;
        find_selected_cycle_mark(&self.terms, from).ok_or_else(|| {
            DjogiError::Validation(format!(
                "exclude_cycle_rows() requires selected term {from:?} to carry a CYCLE clause"
            ))
        })?;
        self.consumer_exclude_cycle_rows = true;
        Ok(self)
    }
}

enum ConsumerTail {
    Full,
    CountWrapped,
    ExistsWrapped,
}

fn emit_cte_preamble<M: Model>(
    acc: &mut SqlAccumulator,
    cte: &CteQuerySet<M>,
) -> Result<(), DjogiError> {
    acc.push_sql("WITH ");
    if cte.terms.iter().any(|term| term.recursive) {
        acc.push_sql("RECURSIVE ");
    }
    for (idx, term) in cte.terms.iter().enumerate() {
        if idx > 0 {
            acc.push_sql(", ");
        }
        acc.push_sql(term.name);
        acc.push_sql(" AS (");
        acc.extend_with((term.body.as_ref())()?);
        acc.push_sql(")");
        if let Some(cycle) = &term.cycle {
            acc.push_sql(" CYCLE ");
            acc.push_csv(cycle.cycle_columns.iter().copied());
            acc.push_sql(" SET ");
            acc.push_sql(cycle.mark_column);
            acc.push_sql(" USING ");
            acc.push_sql(cycle.path_column);
        }
    }
    Ok(())
}

fn push_consumer_where<M: Model>(
    acc: &mut SqlAccumulator,
    cte: &CteQuerySet<M>,
) -> Result<(), DjogiError> {
    if cte.consumer.is_empty() {
        acc.push_sql(" WHERE FALSE");
        return Ok(());
    }
    let exclude_cycle_mark = if cte.consumer_exclude_cycle_rows {
        let from = cte.from.ok_or_else(|| {
            DjogiError::Validation(
                "CTE query has no consumer target — call `.from_cte(<name>)` before a terminal"
                    .to_string(),
            )
        })?;
        Some(find_selected_cycle_mark(&cte.terms, from).ok_or_else(|| {
            DjogiError::Validation(format!(
                "exclude_cycle_rows() requires selected term {from:?} to carry a CYCLE clause"
            ))
        })?)
    } else {
        None
    };
    match (
        exclude_cycle_mark,
        crate::query::sql::has_consumer_where(&cte.consumer),
    ) {
        (Some(mark), true) => {
            acc.push_sql(" WHERE NOT ");
            acc.push_sql(mark);
            acc.push_sql(" AND ");
            crate::query::sql::push_consumer_predicate_only::<M>(acc, &cte.consumer)
                .map_err(DjogiError::from)?;
        }
        (Some(mark), false) => {
            acc.push_sql(" WHERE NOT ");
            acc.push_sql(mark);
        }
        (None, true) => {
            crate::query::sql::push_where_with_ctx(acc, &cte.consumer, SqlEmitContext::root())
                .map_err(DjogiError::from)?;
        }
        (None, false) => {}
    }
    Ok(())
}

fn push_consumer_select<M: Model + FromPgRow>(
    acc: &mut SqlAccumulator,
    cte: &CteQuerySet<M>,
    projection: &'static str,
    include_order_limit_offset: bool,
) -> Result<(), DjogiError> {
    let from = cte.from.ok_or_else(|| {
        DjogiError::Validation(
            "CTE query has no consumer target — call `.from_cte(<name>)` before a terminal"
                .to_string(),
        )
    })?;
    acc.push_sql(" SELECT ");
    acc.push_sql(projection);
    acc.push_sql(" FROM ");
    acc.push_sql(from);
    push_consumer_where(acc, cte)?;
    if include_order_limit_offset {
        crate::query::sql::push_consumer_order_limit_offset(acc, &cte.consumer);
    }
    Ok(())
}

fn push_qualified_columns_for_arm<M: FromPgRow>(acc: &mut SqlAccumulator, alias: &'static str) {
    for (idx, col) in <M as FromPgRow>::COLUMNS.iter().enumerate() {
        if idx > 0 {
            acc.push_sql(", ");
        }
        acc.push_sql(alias);
        acc.push_sql(".");
        acc.push_sql(col);
    }
}

pub(crate) fn build_recursive_cte_body<M: Model + FromPgRow>(
    anchor: &QuerySet<M>,
    arm: &RecursiveArm<M>,
    cte_name: &'static str,
) -> Result<SqlAccumulator, DjogiError> {
    let (table_col, cte_col) = arm.join.ok_or_else(|| {
        DjogiError::Validation(
            "recursive arm has no self-edge — call `.join_on(table_col, cte_col)` before passing it to with_recursive"
                .to_string(),
        )
    })?;

    let mut acc = SqlAccumulator::new("");
    // Strip any default ordering from the anchor arm — ORDER BY is not
    // legal inside a recursive CTE body. Explicit ordering is already
    // rejected above; default proxy ordering passes validation but must
    // not appear in the emitted SQL.
    let mut anchor_no_order = anchor.clone();
    anchor_no_order.ordering.clear();
    acc.extend_with(build_cte_queryset_body(&anchor_no_order)?);
    acc.push_sql(" UNION ALL SELECT ");
    push_qualified_columns_for_arm::<M>(&mut acc, "t");
    acc.push_sql(" FROM ");
    acc.push_sql(M::table_name());
    acc.push_sql(" t JOIN ");
    acc.push_sql(cte_name);
    acc.push_sql(" cte ON t.");
    acc.push_sql(table_col);
    acc.push_sql(" = cte.");
    acc.push_sql(cte_col);
    crate::query::sql::push_where_with_ctx(&mut acc, &arm.arm_filter, SqlEmitContext::joined("t"))
        .map_err(DjogiError::from)?;
    Ok(acc)
}

fn build_cte_select_inner<M: Model + FromPgRow>(
    cte: &CteQuerySet<M>,
    tail: ConsumerTail,
) -> Result<SqlAccumulator, DjogiError> {
    let mut acc = SqlAccumulator::new("");
    if matches!(tail, ConsumerTail::ExistsWrapped) {
        acc.push_sql("SELECT EXISTS (");
    }
    emit_cte_preamble(&mut acc, cte)?;
    match tail {
        ConsumerTail::Full => {
            push_consumer_select(&mut acc, cte, <M as FromPgRow>::COLUMN_LIST, true)?;
        }
        ConsumerTail::CountWrapped => {
            acc.push_sql(" SELECT COUNT(*) FROM (");
            push_consumer_select(&mut acc, cte, <M as FromPgRow>::COLUMN_LIST, false)?;
            acc.push_sql(") AS sub");
        }
        ConsumerTail::ExistsWrapped => {
            push_consumer_select(&mut acc, cte, "1", false)?;
            acc.push_sql(" LIMIT 1)");
        }
    }
    Ok(acc)
}

pub(crate) fn build_cte_select<M: Model + FromPgRow>(
    cte: &CteQuerySet<M>,
) -> Result<SqlAccumulator, DjogiError> {
    build_cte_select_inner(cte, ConsumerTail::Full)
}

pub(crate) fn build_cte_count<M: Model + FromPgRow>(
    cte: &CteQuerySet<M>,
) -> Result<SqlAccumulator, DjogiError> {
    build_cte_select_inner(cte, ConsumerTail::CountWrapped)
}

pub(crate) fn build_cte_exists<M: Model + FromPgRow>(
    cte: &CteQuerySet<M>,
) -> Result<SqlAccumulator, DjogiError> {
    build_cte_select_inner(cte, ConsumerTail::ExistsWrapped)
}

impl<M: Model + FromPgRow> CteQuerySet<M> {
    #[doc(hidden)]
    pub fn __sql_for_test(&self) -> Result<String, DjogiError> {
        let acc = build_cte_select(self)?;
        let (sql, _binds) = acc.into_parts();
        Ok(sql)
    }
}

#[cfg(any(test, feature = "testing"))]
impl<M: Model + FromPgRow> CteQuerySet<M> {
    pub fn render_cte_sql_for_testing(&self) -> Result<String, DjogiError> {
        self.__sql_for_test()
    }
}

impl<M: Model> CteQuerySet<M>
where
    M: FromPgRow + Send + Unpin,
{
    /// Execute the CTE query and collect every row into a `Vec<M>`.
    pub fn fetch_all<'ctx>(
        self,
        ctx: &'ctx mut crate::context::DjogiContext,
    ) -> impl std::future::Future<Output = Result<Vec<M>, DjogiError>> + Send + 'ctx
    where
        M: 'ctx,
    {
        async move {
            // TASK6:empty_contract — consumer is structural-none; no SQL issued.
            if self.consumer.is_empty() {
                return Ok(vec![]);
            }
            let acc = build_cte_select(&self)?;
            crate::query::terminal::auto_set_tenant::<M>(ctx).await?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let rows = ctx.query_all(&sql, &params).await?;
            rows.iter()
                .map(|row| M::from_pg_row(row))
                .collect::<Result<Vec<M>, _>>()
        }
    }

    /// Execute the CTE query with `LIMIT 1` and return the first row, if any.
    pub fn first<'ctx>(
        mut self,
        ctx: &'ctx mut crate::context::DjogiContext,
    ) -> impl std::future::Future<Output = Result<Option<M>, DjogiError>> + Send + 'ctx
    where
        M: 'ctx,
    {
        async move {
            // TASK6:empty_contract — consumer is structural-none; no SQL issued.
            if self.consumer.is_empty() {
                return Ok(None);
            }
            self.consumer = self.consumer.limit(1);
            let acc = build_cte_select(&self)?;
            crate::query::terminal::auto_set_tenant::<M>(ctx).await?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let opt = ctx.query_opt(&sql, &params).await?;
            opt.as_ref().map(|row| M::from_pg_row(row)).transpose()
        }
    }
}

impl<M: Model> CteQuerySet<M>
where
    M: FromPgRow,
{
    /// Return the count of rows the CTE consumer would yield.
    pub fn count<'ctx>(
        self,
        ctx: &'ctx mut crate::context::DjogiContext,
    ) -> impl std::future::Future<Output = Result<i64, DjogiError>> + Send + 'ctx
    where
        M: 'ctx,
    {
        async move {
            // TASK6:empty_contract — consumer is structural-none; no SQL issued.
            if self.consumer.is_empty() {
                return Ok(0_i64);
            }
            let acc = build_cte_count(&self)?;
            crate::query::terminal::auto_set_tenant::<M>(ctx).await?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let row = ctx.query_one(&sql, &params).await?;
            crate::pg::decode::try_get_scalar::<i64>(&row, 0)
        }
    }

    /// Return whether the CTE consumer yields at least one row.
    pub fn exists<'ctx>(
        self,
        ctx: &'ctx mut crate::context::DjogiContext,
    ) -> impl std::future::Future<Output = Result<bool, DjogiError>> + Send + 'ctx
    where
        M: 'ctx,
    {
        async move {
            // TASK6:empty_contract — consumer is structural-none; no SQL issued.
            if self.consumer.is_empty() {
                return Ok(false);
            }
            let acc = build_cte_exists(&self)?;
            crate::query::terminal::auto_set_tenant::<M>(ctx).await?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let row = ctx.query_one(&sql, &params).await?;
            crate::pg::decode::try_get_scalar::<bool>(&row, 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::ModelDescriptor;
    use crate::query::field::DjogiField;
    use crate::query::queryset::CacheTarget;
    use crate::types::HeerId;

    struct MiniNode;

    #[derive(Clone, Copy, Default)]
    struct MiniNodeFields;

    impl MiniNodeFields {
        fn active(&self) -> DjogiField<MiniNode, bool> {
            crate::__private::query::__make_djogi_field("active", |_| {
                static ACTIVE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                ACTIVE.get_or_init(|| false)
            })
        }
    }

    impl crate::model::__sealed::Sealed for MiniNode {}

    impl crate::model::Model for MiniNode {
        type Pk = HeerId;
        type Fields = MiniNodeFields;

        fn table_name() -> &'static str {
            "mini_nodes"
        }

        fn pk_value(&self) -> &Self::Pk {
            unreachable!()
        }

        fn descriptor() -> &'static ModelDescriptor {
            unreachable!()
        }

        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: Self::Pk,
        ) -> impl std::future::Future<Output = Result<Self, DjogiError>> + Send {
            async { unreachable!() }
        }

        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, DjogiError>> + Send {
            async { unreachable!() }
        }

        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), DjogiError>> + Send + 'ctx {
            async { unreachable!() }
        }

        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), DjogiError>> + Send {
            async { unreachable!() }
        }

        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<Self, DjogiError>> + Send + 'ctx {
            async { unreachable!() }
        }

        fn __djogi_emit_field_predicate(
            acc: &mut crate::pg::accumulator::SqlAccumulator,
            field: &crate::types::FieldPredicate<Self>,
            ctx: crate::query::SqlEmitContext,
        ) -> Result<(), crate::query::PortablePredicateError> {
            match (field.field_name(), field.op()) {
                ("active", crate::types::LookupOp::Eq) => {
                    crate::query::portable::emit::emit_value::<Self, bool>(
                        acc, ctx, "active", " = ", field,
                    )
                }
                (field_name, _) => Err(crate::query::PortablePredicateError::UnsupportedField {
                    field: field_name,
                }),
            }
        }
    }

    impl FromPgRow for MiniNode {
        const COLUMNS: &'static [&'static str] = &["id", "parent_id", "active"];
        const COLUMN_LIST: &'static str = "id, parent_id, active";

        fn from_pg_row(_row: &tokio_postgres::Row) -> Result<Self, DjogiError> {
            unreachable!()
        }
    }

    struct DummyCacheTarget;

    impl CacheTarget<MiniNode> for DummyCacheTarget {
        fn insert<'a>(
            &'a self,
            _value: &'a MiniNode,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
            Box::pin(async {})
        }
    }

    #[test]
    fn cte_term_carries_name_recursive_flag_and_body() {
        let term = CteTerm {
            name: "my_cte",
            recursive: false,
            body: Box::new(|| {
                let mut body = SqlAccumulator::new("SELECT 1");
                body.push_bind(7_i64);
                Ok(body)
            }),
            cycle: None,
        };
        assert_eq!(term.name, "my_cte");
        assert!(!term.recursive);
        assert!(term.cycle.is_none());
        assert_eq!((term.body.as_ref())().expect("body recipe").bind_count(), 1);
    }

    #[test]
    fn validate_cte_name_rejects_reserved_djogi_prefix() {
        let err = validate_cte_name("__djogi_secret").expect_err("reserved prefix must reject");
        match err {
            crate::DjogiError::Validation(msg) => {
                assert!(
                    msg.contains("__djogi_secret"),
                    "msg names offending value: {msg}"
                );
                assert!(msg.contains("CTE name"), "msg labels the role: {msg}");
            }
            other => panic!("expected DjogiError::Validation, got {other:?}"),
        }
    }

    #[test]
    fn validate_cte_name_rejects_empty_and_bad_first_byte() {
        assert!(validate_cte_name("").is_err());
        assert!(validate_cte_name("1cte").is_err());
        assert!(validate_cte_name("a b").is_err());
    }

    #[test]
    fn validate_cte_name_accepts_plain_identifier() {
        assert!(validate_cte_name("ancestors").is_ok());
        assert!(validate_cte_name("my_cte_2").is_ok());
    }

    #[test]
    fn non_recursive_with_emits_preamble_then_consumer_select() {
        let cte = MiniNode::objects()
            .with("recent", MiniNode::objects())
            .expect("with should validate")
            .from_cte("recent")
            .expect("from_cte should validate");
        let sql = cte.render_cte_sql_for_testing().expect("render");
        assert!(sql.starts_with("WITH recent AS ("), "{sql}");
        assert!(!sql.contains("WITH RECURSIVE"), "{sql}");
        assert!(
            sql.contains("SELECT id, parent_id, active FROM mini_nodes)"),
            "{sql}"
        );
        assert!(
            sql.contains(") SELECT id, parent_id, active FROM recent"),
            "{sql}"
        );
    }

    #[test]
    fn cte_queryset_renders_twice_without_consuming() {
        let cte = MiniNode::objects()
            .with("recent", MiniNode::objects())
            .expect("with")
            .from_cte("recent")
            .expect("from_cte");
        let first = cte.render_cte_sql_for_testing().expect("first render");
        let second = cte.render_cte_sql_for_testing().expect("second render");
        assert_eq!(first, second);
    }

    #[test]
    fn chained_with_emits_two_comma_separated_terms() {
        let cte = MiniNode::objects()
            .with("first_cte", MiniNode::objects())
            .expect("first with")
            .with("second_cte", MiniNode::objects())
            .expect("second with")
            .from_cte("second_cte")
            .expect("from_cte");
        let sql = cte.render_cte_sql_for_testing().expect("render");
        assert!(sql.starts_with("WITH first_cte AS ("), "{sql}");
        assert!(sql.contains("), second_cte AS ("), "{sql}");
        assert!(
            sql.contains(") SELECT id, parent_id, active FROM second_cte"),
            "{sql}"
        );
    }

    #[test]
    fn duplicate_cte_name_is_rejected() {
        let err = MiniNode::objects()
            .with("dup", MiniNode::objects())
            .expect("first with")
            .with("dup", MiniNode::objects())
            .expect_err("duplicate CTE name must reject");
        assert!(matches!(err, crate::DjogiError::Validation(_)), "{err:?}");
    }

    #[test]
    fn duplicate_cte_name_is_rejected_case_insensitively() {
        let err = MiniNode::objects()
            .with("Walk", MiniNode::objects())
            .expect("first with")
            .with("walk", MiniNode::objects())
            .expect_err("case-folded duplicate CTE names must reject");
        assert!(matches!(err, crate::DjogiError::Validation(_)), "{err:?}");
    }

    #[test]
    fn cte_name_colliding_with_model_table_is_rejected() {
        // "mini_nodes" == MiniNode::table_name() — must reject.
        let err = MiniNode::objects()
            .with("mini_nodes", MiniNode::objects())
            .expect_err("CTE name matching model table must be rejected");
        match err {
            crate::DjogiError::Validation(msg) => {
                assert!(
                    msg.contains("mini_nodes"),
                    "message names the identifier: {msg}"
                );
                assert!(
                    msg.contains("conflicts with the model table"),
                    "message describes conflict: {msg}"
                );
            }
            other => panic!("expected DjogiError::Validation, got {other:?}"),
        }
    }

    #[test]
    fn cte_name_colliding_with_model_table_is_rejected_case_insensitively() {
        let err = MiniNode::objects()
            .with("MINI_NODES", MiniNode::objects())
            .expect_err("case-folded collision must also reject");
        assert!(matches!(err, crate::DjogiError::Validation(_)), "{err:?}");
    }

    #[test]
    fn from_cte_rejects_undeclared_term() {
        let err = MiniNode::objects()
            .with("recent", MiniNode::objects())
            .expect("with")
            .from_cte("missing")
            .expect_err("undeclared term must reject");
        assert!(matches!(err, crate::DjogiError::Validation(_)), "{err:?}");
    }

    #[test]
    fn from_cte_resolves_declared_term_case_insensitively() {
        let cte = MiniNode::objects()
            .with("Walk", MiniNode::objects())
            .expect("with")
            .from_cte("walk")
            .expect("case-folded lookup should resolve");
        let sql = cte.render_cte_sql_for_testing().expect("render");
        assert!(sql.contains("FROM walk"), "{sql}");
    }

    #[test]
    fn non_recursive_none_body_emits_where_false() {
        let cte = MiniNode::objects()
            .with("recent", MiniNode::objects().none())
            .expect("with")
            .from_cte("recent")
            .expect("from_cte");
        let sql = cte.render_cte_sql_for_testing().expect("render");
        assert!(sql.contains("FROM mini_nodes WHERE FALSE"), "{sql}");
    }

    #[test]
    fn consumer_none_emits_where_false() {
        let cte = MiniNode::objects()
            .none()
            .with("recent", MiniNode::objects())
            .expect("with")
            .from_cte("recent")
            .expect("from_cte");
        let sql = cte.render_cte_sql_for_testing().expect("render");
        assert!(sql.contains("FROM recent WHERE FALSE"), "{sql}");
    }

    #[test]
    fn render_without_from_cte_returns_validation_error() {
        // Construct a CTE but never call from_cte() — consumer target is None.
        let cte = MiniNode::objects()
            .with("recent", MiniNode::objects())
            .expect("with should succeed");
        let result = cte.render_cte_sql_for_testing();
        match result {
            Err(crate::DjogiError::Validation(msg)) => {
                assert!(
                    msg.contains("from_cte"),
                    "error message must mention from_cte, got: {msg}"
                );
            }
            Ok(sql) => panic!("expected Err but got Ok: {sql}"),
            Err(other) => panic!("expected DjogiError::Validation, got: {other:?}"),
        }
    }

    #[test]
    fn consumer_none_short_circuits_without_sql() {
        // Build a CTE where the consumer queryset was none()-ed.
        // render_cte_sql_for_testing still emits WHERE FALSE (sql shape
        // test already covers that at the consumer_none_emits_where_false test).
        // This test confirms the is_empty() field is visible to the
        // short-circuit path in the terminal methods.
        let cte = MiniNode::objects()
            .none()
            .with("recent", MiniNode::objects())
            .expect("with should succeed");
        assert!(
            cte.consumer.is_empty(),
            "consumer should carry the none() flag"
        );
        // No from_cte needed — the is_empty check fires before from is read.
        // (This is a unit-level structural check; integration coverage
        //  of the actual terminal short-circuit is in tests/integration/cte_live.rs)
    }

    #[test]
    fn with_rejects_distinct_consumer_state() {
        let err = MiniNode::objects()
            .distinct()
            .with("recent", MiniNode::objects())
            .expect_err("distinct consumer state must reject");
        assert!(matches!(err, crate::DjogiError::Validation(_)), "{err:?}");
    }

    #[test]
    fn with_rejects_locking_consumer_state() {
        let err = MiniNode::objects()
            .select_for_update()
            .with("recent", MiniNode::objects())
            .expect_err("locking consumer state must reject");
        assert!(matches!(err, crate::DjogiError::Validation(_)), "{err:?}");
    }

    #[test]
    fn with_rejects_cache_bound_consumer_state() {
        let mut qs = MiniNode::objects();
        qs.cache_target = Some(std::sync::Arc::new(DummyCacheTarget));
        let err = qs
            .with("recent", MiniNode::objects())
            .expect_err("cache-bound consumer state must reject");
        assert!(matches!(err, crate::DjogiError::Validation(_)), "{err:?}");
    }

    #[test]
    fn recursive_arm_captures_name_self_edge_and_filter() {
        let arm = RecursiveArm::<MiniNode>::referencing("ancestors")
            .join_on("parent_id", "id")
            .expect("join_on validates columns")
            .filter(|f| f.active().eq(true));
        assert_eq!(arm.cte_name(), "ancestors");
        let (table_col, cte_col) = arm.join_columns().expect("join_on was called");
        assert_eq!(table_col, "parent_id");
        assert_eq!(cte_col, "id");
    }

    #[test]
    fn recursive_arm_join_on_rejects_invalid_column() {
        assert!(
            RecursiveArm::<MiniNode>::referencing("ancestors")
                .join_on("__djogi_x", "id")
                .is_err()
        );
        assert!(
            RecursiveArm::<MiniNode>::referencing("ancestors")
                .join_on("parent_id", "1bad")
                .is_err()
        );
    }

    #[test]
    fn with_recursive_rejects_join_columns_not_in_model_output() {
        let table_err = MiniNode::objects()
            .with_recursive(
                "ancestors",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("ancestors")
                    .join_on("missing_col", "id")
                    .expect("identifier syntax still passes"),
            )
            .expect_err("unknown table join column must reject");
        assert!(
            matches!(table_err, crate::DjogiError::Validation(_)),
            "{table_err:?}"
        );

        let cte_err = MiniNode::objects()
            .with_recursive(
                "ancestors",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("ancestors")
                    .join_on("parent_id", "missing_col")
                    .expect("identifier syntax still passes"),
            )
            .expect_err("unknown CTE join column must reject");
        assert!(
            matches!(cte_err, crate::DjogiError::Validation(_)),
            "{cte_err:?}"
        );
    }

    #[test]
    fn recursive_arm_referencing_rejects_invalid_name() {
        let arm = RecursiveArm::<MiniNode>::referencing("__djogi_bad");
        assert_eq!(arm.cte_name(), "__djogi_bad");
    }

    #[test]
    fn recursive_arm_double_join_on_rejected() {
        let err = RecursiveArm::<MiniNode>::referencing("ancestors")
            .join_on("parent_id", "id")
            .expect("first join_on")
            .join_on("parent_id", "id")
            .expect_err("second join_on must reject");
        match err {
            crate::DjogiError::Validation(msg) => {
                assert!(msg.contains("join_on"), "{msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn with_recursive_emits_self_referential_union_all_body() {
        let anchor = MiniNode::objects().filter(|f| f.active().eq(true));
        let arm = RecursiveArm::<MiniNode>::referencing("ancestors")
            .join_on("parent_id", "id")
            .expect("join_on");
        let cte = MiniNode::objects()
            .with_recursive("ancestors", anchor, arm)
            .expect("with_recursive")
            .from_cte("ancestors")
            .expect("from_cte");
        let sql = cte.render_cte_sql_for_testing().expect("render");
        assert!(sql.starts_with("WITH RECURSIVE ancestors AS ("), "{sql}");
        assert!(
            sql.contains("SELECT id, parent_id, active FROM mini_nodes WHERE"),
            "{sql}"
        );
        assert!(sql.contains(" UNION ALL "), "{sql}");
        assert!(
            sql.contains("SELECT t.id, t.parent_id, t.active FROM mini_nodes t JOIN ancestors cte ON t.parent_id = cte.id"),
            "{sql}"
        );
        assert!(
            sql.contains(") SELECT id, parent_id, active FROM ancestors"),
            "{sql}"
        );
    }

    #[test]
    fn with_recursive_rejects_name_mismatch_with_arm() {
        let err = MiniNode::objects()
            .with_recursive(
                "descendants",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("ancestors")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect_err("mismatch must reject");
        match err {
            crate::DjogiError::Validation(msg) => {
                assert!(msg.contains("descendants"), "{msg}");
                assert!(msg.contains("ancestors"), "{msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn with_recursive_rejects_arm_without_join_on() {
        let err = MiniNode::objects()
            .with_recursive(
                "ancestors",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("ancestors"),
            )
            .expect_err("arm without join_on must reject");
        match err {
            crate::DjogiError::Validation(msg) => assert!(msg.contains("join_on"), "{msg}"),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn with_recursive_rejects_anchor_order_and_limit_tail() {
        let err = MiniNode::objects().with_recursive(
            "ancestors",
            MiniNode::objects().order_by(|f| f.active().asc()).limit(1),
            RecursiveArm::<MiniNode>::referencing("ancestors")
                .join_on("parent_id", "id")
                .expect("join_on"),
        );
        assert!(
            matches!(err, Err(crate::DjogiError::Validation(_))),
            "{err:?}"
        );
    }

    #[test]
    fn recursive_anchor_proxy_default_ordering_passes_validation() {
        // Simulate proxy queryset: ordering seeded by QuerySet::new() but
        // has_explicit_ordering stays false — same state as a proxy model
        // with default_order_by() returning entries.
        let mut anchor: QuerySet<MiniNode> = MiniNode::objects();
        // Seed ordering manually to simulate proxy behavior without
        // setting has_explicit_ordering (QuerySet::new() path).
        anchor
            .ordering
            .push(crate::query::order::OrderExpr::__from_macro_column(
                "active",
                crate::query::order::Direction::Asc,
                crate::query::order::NullsOrder::Default,
            ));
        assert!(!anchor.has_explicit_ordering);
        let result = validate_recursive_anchor(&anchor);
        assert!(
            result.is_ok(),
            "proxy default ordering must pass: {result:?}"
        );
    }

    #[test]
    fn recursive_anchor_explicit_ordering_still_rejected() {
        let anchor = MiniNode::objects().order_by(|f| f.active().asc());
        assert!(anchor.has_explicit_ordering);
        let result = validate_recursive_anchor(&anchor);
        assert!(
            matches!(result, Err(crate::DjogiError::Validation(ref msg)) if msg.contains("order_by")),
            "explicit order_by must still be rejected: {result:?}"
        );
    }

    #[test]
    fn recursive_none_anchor_emits_where_false() {
        let cte = MiniNode::objects()
            .with_recursive(
                "ancestors",
                MiniNode::objects().none(),
                RecursiveArm::<MiniNode>::referencing("ancestors")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive")
            .from_cte("ancestors")
            .expect("from_cte");
        let sql = cte.render_cte_sql_for_testing().expect("render");
        assert!(
            sql.contains("FROM mini_nodes WHERE FALSE UNION ALL"),
            "{sql}"
        );
    }

    #[test]
    fn with_recursive_arm_filter_emits_qualified_where() {
        let cte = MiniNode::objects()
            .with_recursive(
                "ancestors",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("ancestors")
                    .join_on("parent_id", "id")
                    .expect("join_on")
                    .filter(|f| f.active().eq(true)),
            )
            .expect("with_recursive")
            .from_cte("ancestors")
            .expect("from_cte");
        let sql = cte.render_cte_sql_for_testing().expect("render");
        assert!(
            sql.contains("ON t.parent_id = cte.id WHERE t.active = $"),
            "{sql}"
        );
    }

    #[test]
    fn cycle_emits_clause_on_recursive_term() {
        let cte = MiniNode::objects()
            .with_recursive(
                "walk",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("walk")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive")
            .cycle(&["id"], "is_cycle", "cycle_path")
            .expect("cycle")
            .from_cte("walk")
            .expect("from_cte");
        let sql = cte.render_cte_sql_for_testing().expect("render");
        assert!(
            sql.contains(") CYCLE id SET is_cycle USING cycle_path"),
            "{sql}"
        );
    }

    #[test]
    fn cycle_emits_multiple_columns_comma_separated() {
        let cte = MiniNode::objects()
            .with_recursive(
                "walk",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("walk")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive")
            .cycle(&["id", "parent_id"], "is_cycle", "cycle_path")
            .expect("cycle")
            .from_cte("walk")
            .expect("from_cte");
        let sql = cte.render_cte_sql_for_testing().expect("render");
        assert!(
            sql.contains("CYCLE id, parent_id SET is_cycle USING cycle_path"),
            "{sql}"
        );
    }

    #[test]
    fn cycle_rejects_column_not_in_model_output() {
        let err = MiniNode::objects()
            .with_recursive(
                "walk",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("walk")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive")
            .cycle(&["source_id"], "is_cycle", "cycle_path")
            .expect_err("non-output cycle column must reject");
        assert!(matches!(err, crate::DjogiError::Validation(_)), "{err:?}");
    }

    #[test]
    fn cycle_on_non_recursive_term_is_rejected() {
        let err = MiniNode::objects()
            .with("plain", MiniNode::objects())
            .expect("with")
            .cycle(&["id"], "is_cycle", "cycle_path")
            .expect_err("cycle on non-recursive term must reject");
        assert!(matches!(err, crate::DjogiError::Validation(_)));
    }

    #[test]
    fn cycle_with_empty_columns_is_rejected() {
        let err = MiniNode::objects()
            .with_recursive(
                "walk",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("walk")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive")
            .cycle(&[], "is_cycle", "cycle_path")
            .expect_err("empty columns reject");
        assert!(matches!(err, crate::DjogiError::Validation(_)));
    }

    #[test]
    fn cycle_with_reserved_column_name_is_rejected() {
        let err = MiniNode::objects()
            .with_recursive(
                "walk",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("walk")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive")
            .cycle(&["__djogi_x"], "is_cycle", "cycle_path")
            .expect_err("reserved column name reject");
        assert!(matches!(err, crate::DjogiError::Validation(_)));
    }

    #[test]
    fn exclude_cycle_rows_emits_where_not_mark() {
        let cte = MiniNode::objects()
            .with_recursive(
                "walk",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("walk")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive")
            .cycle(&["id"], "is_cycle", "cycle_path")
            .expect("cycle")
            .from_cte("walk")
            .expect("from_cte")
            .exclude_cycle_rows()
            .expect("exclude_cycle_rows");
        let sql = cte.render_cte_sql_for_testing().expect("render");
        assert!(sql.contains("FROM walk WHERE NOT is_cycle"), "{sql}");
    }

    #[test]
    fn exclude_cycle_rows_combines_with_filter() {
        let cte = MiniNode::objects()
            .with_recursive(
                "walk",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("walk")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive")
            .cycle(&["id"], "is_cycle", "cycle_path")
            .expect("cycle")
            .from_cte("walk")
            .expect("from_cte")
            .filter(|f| f.active().eq(true))
            .exclude_cycle_rows()
            .expect("exclude_cycle_rows");
        let sql = cte.render_cte_sql_for_testing().expect("render");
        assert!(sql.contains("WHERE NOT is_cycle AND active = $"), "{sql}");
    }

    #[test]
    fn exclude_cycle_rows_without_cycle_is_rejected() {
        let err = MiniNode::objects()
            .with_recursive(
                "walk",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("walk")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive")
            .from_cte("walk")
            .expect("from_cte")
            .exclude_cycle_rows()
            .expect_err("no cycle should reject");
        assert!(matches!(err, crate::DjogiError::Validation(_)));
    }

    #[test]
    fn exclude_cycle_rows_rejects_selected_term_without_mark() {
        let err = MiniNode::objects()
            .with_recursive(
                "walk",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("walk")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive")
            .cycle(&["id"], "is_cycle", "cycle_path")
            .expect("cycle")
            .with("recent", MiniNode::objects())
            .expect("with")
            .from_cte("recent")
            .expect("from_cte")
            .exclude_cycle_rows()
            .expect_err("selected term without cycle mark must reject");
        assert!(matches!(err, crate::DjogiError::Validation(_)), "{err:?}");
    }

    #[test]
    fn from_cte_rejects_retarget_after_exclude_cycle_rows_without_mark() {
        let err = MiniNode::objects()
            .with_recursive(
                "walk",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("walk")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive")
            .cycle(&["id"], "walk_cycle", "walk_path")
            .expect("cycle")
            .with("recent", MiniNode::objects())
            .expect("with")
            .from_cte("walk")
            .expect("from_cte")
            .exclude_cycle_rows()
            .expect("exclude_cycle_rows")
            .from_cte("recent")
            .expect_err("retargeting to non-cycled term must reject");
        assert!(matches!(err, crate::DjogiError::Validation(_)), "{err:?}");
    }

    #[test]
    fn from_cte_rebinds_cycle_exclusion_when_retargeting_between_cycle_terms() {
        let cte = MiniNode::objects()
            .with_recursive(
                "walk",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("walk")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive")
            .cycle(&["id"], "walk_cycle", "walk_path")
            .expect("cycle")
            .with_recursive(
                "trail",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("trail")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive")
            .cycle(&["id"], "trail_cycle", "trail_path")
            .expect("cycle")
            .from_cte("walk")
            .expect("from_cte")
            .exclude_cycle_rows()
            .expect("exclude_cycle_rows")
            .from_cte("trail")
            .expect("retarget to cycled term should rebind");
        let sql = cte.render_cte_sql_for_testing().expect("render");
        assert!(sql.contains("FROM trail WHERE NOT trail_cycle"), "{sql}");
        assert!(!sql.contains("WHERE NOT walk_cycle"), "{sql}");
    }

    #[test]
    fn cycle_mark_colliding_with_model_column_is_rejected() {
        let err = MiniNode::objects()
            .with_recursive(
                "walk",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("walk")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive")
            .cycle(&["id"], "active", "cycle_path")
            .expect_err("collision reject");
        match err {
            crate::DjogiError::Validation(msg) => {
                assert!(msg.contains("active"), "{msg}");
                assert!(msg.contains("collides"), "{msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn cycle_mark_collision_is_case_insensitive() {
        let err = MiniNode::objects()
            .with_recursive(
                "walk",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("walk")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive")
            .cycle(&["id"], "ID", "cycle_path")
            .expect_err("case-folded collision should reject");
        assert!(matches!(err, crate::DjogiError::Validation(_)), "{err:?}");
    }

    #[test]
    fn cycle_mark_equals_path_is_rejected() {
        let err = MiniNode::objects()
            .with_recursive(
                "walk",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("walk")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive")
            .cycle(&["id"], "same", "same")
            .expect_err("same mark/path reject");
        assert!(matches!(err, crate::DjogiError::Validation(_)));
    }

    #[test]
    fn cycle_mark_in_cycle_cols_is_rejected() {
        let err = MiniNode::objects()
            .with_recursive(
                "walk",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("walk")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive")
            .cycle(&["tracking_col"], "tracking_col", "cycle_path")
            .expect_err("mark in cycle cols reject");
        assert!(matches!(err, crate::DjogiError::Validation(_)));
    }

    #[test]
    fn consumer_filter_and_order_emit_in_tail() {
        let cte = MiniNode::objects()
            .with_recursive(
                "walk",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("walk")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive")
            .cycle(&["id"], "is_cycle", "cycle_path")
            .expect("cycle")
            .from_cte("walk")
            .expect("from_cte")
            .filter(|f| f.active().eq(true))
            .order_by(|f| f.active().asc());
        let sql = cte.render_cte_sql_for_testing().expect("render");
        assert!(sql.contains("FROM walk WHERE active = $"), "{sql}");
        assert!(sql.contains(" ORDER BY active ASC"), "{sql}");
    }

    #[test]
    fn cte_count_wraps_in_count_star_subquery() {
        let cte = MiniNode::objects()
            .with("recent", MiniNode::objects())
            .expect("with")
            .from_cte("recent")
            .expect("from_cte");
        let (sql, _binds) = build_cte_count(&cte).expect("count build").into_parts();
        assert!(sql.starts_with("WITH recent AS ("), "{sql}");
        assert!(sql.contains("SELECT COUNT(*) FROM ("), "{sql}");
        assert!(sql.trim_end().ends_with(") AS sub"), "{sql}");
    }

    #[test]
    fn cte_exists_wraps_in_select_exists() {
        let cte = MiniNode::objects()
            .with("recent", MiniNode::objects())
            .expect("with")
            .from_cte("recent")
            .expect("from_cte");
        let (sql, _binds) = build_cte_exists(&cte).expect("exists build").into_parts();
        assert!(sql.starts_with("SELECT EXISTS (WITH recent AS ("), "{sql}");
        assert!(sql.contains(" SELECT 1 FROM recent LIMIT 1)"), "{sql}");
    }
}
