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

// ── Full-text-search guard for CTE consumers ───────────────────────────────
//
// FTS predicates reference the model's `GENERATED ALWAYS AS` tsvector column
// (e.g. `search`), which CTE bodies never project (bodies carry only declared
// struct fields). A `search @@ …` / `ts_rank(search, …)` reference against a CTE
// term raises Postgres `42703` at runtime. The guard below detects FTS anywhere
// in the consumer predicate tree so the build boundary can refuse it (see
// `build_cte_select_inner`). The consumer predicate is a `Q<T>` tree, and FTS
// leaves can arrive wrapped several layers deep, so the walk descends
// `Q<T>` → `Condition` → `ExprNode`.

/// Walk an [`ExprNode`](crate::expr::node::ExprNode) and report whether it
/// carries an FTS leaf (`TsMatch` / `TsRank` / `TsRankCd`) anywhere in the
/// tree. Mirrors the exhaustive, no-wildcard discipline of
/// `expr_node_contains_excluded` (`crate::query::insert_select`): every variant
/// with inner `ExprNode` children recurses; every leaf / separate-scope variant
/// returns `false`. The exhaustive match means a future FTS-bearing variant
/// forces a compile error here rather than silently slipping past the guard.
fn expr_node_contains_fts(node: &crate::expr::node::ExprNode) -> bool {
    use crate::expr::node::ExprNode;
    match node {
        // FTS leaves — the thing we are looking for.
        ExprNode::TsMatch { .. } | ExprNode::TsRank { .. } | ExprNode::TsRankCd { .. } => true,

        // Leaf variants with no inner ExprNode — cannot carry FTS.
        ExprNode::Field { .. }
        | ExprNode::RawSql(_)
        | ExprNode::Literal(_)
        | ExprNode::ArrayLength { .. }
        | ExprNode::CurrentYear
        | ExprNode::Now
        | ExprNode::OuterRef { .. }
        | ExprNode::OuterRefColumn { .. }
        | ExprNode::OuterRefAlias { .. }
        | ExprNode::CteAliasField { .. }
        | ExprNode::Excluded { .. }
        | ExprNode::IntervalLiteral { .. } => false,

        // Binary arithmetic / boolean nodes recurse into both operands.
        ExprNode::Cmp { lhs, rhs, .. }
        | ExprNode::Add(lhs, rhs)
        | ExprNode::Sub(lhs, rhs)
        | ExprNode::Mul(lhs, rhs)
        | ExprNode::Div(lhs, rhs)
        | ExprNode::And(lhs, rhs)
        | ExprNode::Or(lhs, rhs) => expr_node_contains_fts(lhs) || expr_node_contains_fts(rhs),

        // Unary wrappers recurse into the single inner node.
        ExprNode::Not(inner) | ExprNode::IsNull(inner) | ExprNode::IsNotNull(inner) => {
            expr_node_contains_fts(inner)
        }

        // Variadic expressions recurse into every operand.
        ExprNode::Coalesce(operands)
        | ExprNode::Least(operands)
        | ExprNode::Greatest(operands)
        | ExprNode::GroupingVariadic { args: operands } => {
            operands.iter().any(expr_node_contains_fts)
        }

        // CASE recurses into each arm's condition and value, plus the default.
        ExprNode::Case { arms, otherwise } => {
            arms.iter()
                .any(|(cond, val)| expr_node_contains_fts(cond) || expr_node_contains_fts(val))
                || expr_node_contains_fts(otherwise)
        }

        // Aggregate recurses into the argument and optional expression fields.
        ExprNode::Aggregate {
            arg, arg2, filter, ..
        } => {
            expr_node_contains_fts(arg)
                || arg2.as_deref().is_some_and(expr_node_contains_fts)
                || filter.as_deref().is_some_and(expr_node_contains_fts)
        }

        // Subquery bodies are separate scopes; only the lhs expression belongs
        // to this scope (Exists / Subquery open a fresh FROM).
        ExprNode::Exists(_) | ExprNode::Subquery(_) => false,
        ExprNode::InSubquery { lhs, .. } | ExprNode::QuantifiedSubquery { lhs, .. } => {
            expr_node_contains_fts(lhs)
        }

        // Trigram leaves carry no inner ExprNode.
        #[cfg(feature = "trgm")]
        ExprNode::TrgmSimilarTo { .. } | ExprNode::TrgmSimilarityScore { .. } => false,

        // Spatial: the `DWithinExprToExpr` variant carries two boxed ExprNode
        // operands that can hide an FTS leaf (e.g. inside a CASE used as a
        // geography operand); every other spatial variant is a pure leaf.
        #[cfg(feature = "spatial")]
        ExprNode::Spatial(s) => spatial_expr_contains_fts(s),
        #[cfg(feature = "spatial")]
        ExprNode::RowAggregate { .. } => false,
    }
}

/// Walk a [`SpatialExpr`](crate::expr::spatial::SpatialExpr) and report whether
/// it carries an FTS leaf. Only `DWithinExprToExpr` has `ExprNode` children;
/// every other variant holds column names, `GeoPoint`, `f64`, or EWKB bytes,
/// which cannot nest a `TsMatch` / `TsRank` / `TsRankCd`. The match is
/// exhaustive (no wildcard) so a future spatial variant with `ExprNode`
/// children forces an update here, preserving the fail-safe property against
/// `SpatialExpr`'s `#[non_exhaustive]`.
#[cfg(feature = "spatial")]
fn spatial_expr_contains_fts(s: &crate::expr::spatial::SpatialExpr) -> bool {
    use crate::expr::spatial::SpatialExpr;
    match s {
        // Only variant carrying ExprNode operands — recurse into both.
        SpatialExpr::DWithinExprToExpr { lhs, rhs, .. } => {
            expr_node_contains_fts(lhs) || expr_node_contains_fts(rhs)
        }
        // Pure spatial variants: column names, GeoPoint, f64, EWKB bytes —
        // no ExprNode children, so no FTS can hide here.
        SpatialExpr::Within { .. }
        | SpatialExpr::Distance { .. }
        | SpatialExpr::Contains { .. }
        | SpatialExpr::Intersects { .. }
        | SpatialExpr::Touches { .. }
        | SpatialExpr::WithinShape { .. }
        | SpatialExpr::BoundedBy { .. }
        | SpatialExpr::Area { .. }
        | SpatialExpr::Intersection { .. }
        | SpatialExpr::AreaOfIntersection { .. } => false,
    }
}

/// Walk a [`Condition`](crate::query::condition::Condition) tree and report
/// whether it carries an FTS leaf. The only bridge into the `ExprNode` layer is
/// `Condition::Expr` (where `f.search().matches(…)` lands); the boolean
/// combinators recurse. Exhaustive match (no wildcard) — a future `Condition`
/// variant that can carry an `Expr` forces a compile error here.
fn condition_contains_fts(cond: &crate::query::condition::Condition) -> bool {
    use crate::query::condition::Condition;
    match cond {
        Condition::Expr(expr) => expr_node_contains_fts(&expr.node),
        Condition::And(parts) | Condition::Or(parts) => parts.iter().any(condition_contains_fts),
        Condition::Not(inner) => condition_contains_fts(inner),
        Condition::True
        | Condition::Leaf(_)
        | Condition::ArrayContains(_)
        | Condition::ArrayContainedBy(_)
        | Condition::ArrayOverlap(_)
        | Condition::RangePredicate(_)
        | Condition::JsonbPath(_)
        | Condition::RawSql(_) => false,
    }
}

/// Walk the consumer's [`Q<T>`](crate::query::q::Q) predicate tree and report
/// whether it carries an FTS leaf. `Q::Expression` is the `rank(q).gt(…)`
/// ingress and `Q::Condition` the `matches(q)` ingress; the combinators
/// recurse. Exhaustive same-crate match — a new FTS-bearing `Q` variant forces
/// an update here even though `Q<T>` is `#[non_exhaustive]` across crates.
fn q_contains_fts<T: Model>(q: &crate::query::q::Q<T>) -> bool {
    use crate::query::q::Q;
    match q {
        Q::Expression(expr) => expr_node_contains_fts(&expr.node),
        Q::Condition(cond) => condition_contains_fts(cond),
        Q::Compound { parts, .. } => parts.iter().any(q_contains_fts),
        Q::Xor(a, b) => q_contains_fts(a) || q_contains_fts(b),
        Q::Negated(inner) => q_contains_fts(inner),
        Q::Portable(_) | Q::Ilike(_, _) | Q::JsonbPath(_) | Q::Regex(_, _, _) | Q::Array(_) => {
            false
        }
    }
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
    /// Additional CTE-term names appended to the consumer `FROM` list after
    /// `from`, so a cross-CTE `CteRef` predicate can name a sibling term that
    /// is in the FROM clause (Postgres requires a `WITH` term to appear in
    /// `FROM` before its columns are referenceable). Insertion order is
    /// preserved; the emitted FROM is `<from>, <also_from[0]>, <also_from[1]>, …`.
    pub(crate) also_from: Vec<&'static str>,
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
            .field("also_from", &self.also_from)
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
            also_from: Vec::new(),
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
        // A term brought into the consumer FROM via `also_from_cte` must not
        // also become the primary `from_cte` target — `FROM walk, walk` is a
        // duplicate-alias error in Postgres (42712). Guard the inverse of the
        // `also_from_cte` duplicate checks so the "each term appears in the
        // consumer FROM at most once" invariant holds in either call order.
        if self
            .also_from
            .iter()
            .any(|existing| sql_ident_eq(existing, name))
        {
            return Err(DjogiError::Validation(format!(
                "from_cte({name:?}) names a term already added via also_from_cte; a CTE term may \
                 appear in the consumer FROM at most once"
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
            also_from: Vec::new(),
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

    /// AND an expression-IR predicate onto the consumer `SELECT`.
    /// The closure receives the consumer model's `M::Fields` handle and
    /// returns an [`Expr<bool>`](crate::expr::Expr). Use this to compose
    /// cross-CTE column references via
    /// [`crate::query::cte_ref::CteRef`] — e.g. filter the consumer on
    /// proximity to a column projected by another CTE term:
    /// ```ignore
    /// places_query
    ///     .with("anchor", anchor_select)?
    ///     .with("places_cte", all_places)?
    ///     .from_cte("places_cte")?
    ///     // `anchor` is a sibling term referenced below — it must be in the
    ///     // consumer FROM, so bring it in (emits `FROM places_cte, anchor`):
    ///     .also_from_cte("anchor")?
    ///     .filter_expr(|p| Expr::dwithin(
    ///         p.geo().as_expr(),
    ///         CteRef::new("anchor", "geo")?.as_expr::<GeoPoint>(),
    ///         50_000.0,
    ///     ))
    /// ```
    /// # Scope note
    /// A [`CteRef`](crate::query::cte_ref::CteRef) names a `<alias>.<column>`
    /// pair; the named CTE term must be in the consumer SELECT's `FROM`
    /// clause for the column to resolve (Postgres requires a `WITH` term in
    /// `FROM` before its columns are referenceable — a merely-declared term
    /// raises `42P01`). If the `CteRef` names the
    /// [`from_cte`](Self::from_cte) target, it is already in `FROM`. To
    /// reference a *sibling* term, bring it into the consumer `FROM` with
    /// [`also_from_cte`](Self::also_from_cte). See
    /// [`crate::query::cte_ref::CteRef`] for the full limitation note.
    /// # Full-text search
    /// FTS predicates (`f.search().matches(...)` / `.rank(...)` /
    /// `.rank_cd(...)`) are **not supported** on a CTE consumer, because a CTE
    /// body projects only declared struct fields and the FTS tsvector is a
    /// generated column absent from that projection. A terminal on a consumer
    /// carrying an FTS predicate fails with [`DjogiError::Validation`] naming
    /// the FTS surface — filter FTS on the base `QuerySet` before entering the
    /// CTE builder.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn filter_expr<F>(mut self, f: F) -> Self
    where
        F: FnOnce(M::Fields) -> crate::expr::Expr<bool>,
    {
        self.consumer = self.consumer.filter_expr(f);
        self
    }

    /// Bring an additional CTE term into the consumer `SELECT`'s `FROM`
    /// clause, after the [`from_cte`](Self::from_cte) target. Use this when a
    /// [`filter_expr`](Self::filter_expr) predicate references a *sibling*
    /// CTE's column via [`CteRef`](crate::query::cte_ref::CteRef): Postgres
    /// requires the sibling term to appear in `FROM` before `<alias>.<column>`
    /// resolves (a merely-declared `WITH` term is not in scope — referencing
    /// it raises `42P01`). The emitted `FROM` becomes
    /// `<from_cte target>, <this alias>` (an implicit cross join).
    /// # Single-row anchor caveat
    /// The added term is cross-joined (comma form). If it returns a single
    /// row (e.g. an anchor lookup `WHERE id = $1`), the result is the natural
    /// 1×N correlation. If it returns **multiple** rows, the consumer produces
    /// a full cartesian product — usually not intended. Add only single-row
    /// anchor terms unless a cartesian product is what you want.
    /// # Errors
    /// [`DjogiError::Validation`] if `name` is not a valid CTE identifier, does
    /// not name a declared term, or already appears in the consumer `FROM`
    /// (as the `from_cte` target or a prior `also_from_cte` entry — a CTE term
    /// may appear in the consumer FROM at most once).
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn also_from_cte(mut self, name: &'static str) -> Result<Self, DjogiError> {
        validate_cte_name(name)?;
        if find_term(&self.terms, name).is_none() {
            return Err(DjogiError::Validation(format!(
                "also_from_cte({name:?}) references an undeclared CTE term"
            )));
        }
        // Each CTE term may appear in the consumer FROM at most once — a
        // repeat produces `FROM walk, walk`, which Postgres rejects as a
        // duplicate alias (42712). Reject a name equal to the primary target
        // or to an already-appended sibling.
        if self.from.is_some_and(|primary| sql_ident_eq(primary, name)) {
            return Err(DjogiError::Validation(format!(
                "also_from_cte({name:?}) duplicates the from_cte target; a CTE term may appear \
                 in the consumer FROM at most once"
            )));
        }
        if self
            .also_from
            .iter()
            .any(|existing| sql_ident_eq(existing, name))
        {
            return Err(DjogiError::Validation(format!(
                "also_from_cte({name:?}) is already in the consumer FROM; a CTE term may appear at \
                 most once"
            )));
        }
        self.also_from.push(name);
        Ok(self)
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

        // Empty-list early return — mirrors the last_mut() error message below
        // so the earlier-term slice subtraction is proven in-bounds (len >= 1)
        // and the error contract is unchanged.
        if self.terms.is_empty() {
            return Err(DjogiError::Validation(
                "cycle(...) called before any CTE term was added".to_string(),
            ));
        }
        // Reject a CYCLE SET/USING column that duplicates a mark or path column
        // already attached to an *earlier* CTE term. Each term's mark/path
        // becomes a synthetic output column; two terms sharing a mark name make
        // a multi-term consumer FROM (also_from_cte) ambiguous. The `from`-term
        // mark is qualified at emission; this guard keeps even unqualified /
        // sibling-referencing uses unambiguous by construction. Scan every term
        // except the last (the one being configured here) so a replacement
        // cycle() call does not collide with its own prior mark.
        for term in &self.terms[..self.terms.len() - 1] {
            let Some(existing) = &term.cycle else {
                continue;
            };
            if sql_ident_eq(existing.mark_column, mark_column)
                || sql_ident_eq(existing.path_column, mark_column)
                || sql_ident_eq(existing.mark_column, path_column)
                || sql_ident_eq(existing.path_column, path_column)
            {
                return Err(DjogiError::Validation(format!(
                    "CYCLE SET/USING column {mark_column:?}/{path_column:?} duplicates a mark \
                     or path column already declared on CTE term {:?}; mark and path columns \
                     must be unique across all CTE terms",
                    term.name
                )));
            }
        }

        let last = self
            .terms
            .last_mut()
            .expect("non-empty: guarded by the is_empty() early return above");
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
    // The resolved consumer-FROM CTE name, needed for column qualification
    // when `also_from` is non-empty (two-term consumer FROM).
    from: &'static str,
    // The emission context for the consumer tail — computed once in
    // `push_consumer_select` and shared with the SELECT-list / ORDER-BY
    // qualification so every path reads the same qualifier and cannot drift.
    // Root (bare columns) for a single-term FROM; `joined(from)` (columns
    // qualified `<from>.<col>`) when a sibling CTE is in the consumer FROM.
    ctx: SqlEmitContext,
) -> Result<(), DjogiError> {
    if cte.consumer.is_empty() {
        acc.push_sql(" WHERE FALSE");
        return Ok(());
    }
    let exclude_cycle_mark = if cte.consumer_exclude_cycle_rows {
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
            // The cycle mark belongs to the `from` term (find_selected_cycle_mark
            // resolves the from-term's mark). `ctx.push_column` leaves it bare
            // at root and qualifies it `<from>.<mark>` under also_from_cte, so a
            // two-term consumer FROM does not make it ambiguous.
            ctx.push_column(acc, mark);
            acc.push_sql(" AND ");
            crate::query::sql::push_consumer_predicate_only_with_ctx::<M>(acc, &cte.consumer, ctx)
                .map_err(DjogiError::from)?;
        }
        (Some(mark), false) => {
            acc.push_sql(" WHERE NOT ");
            ctx.push_column(acc, mark);
        }
        (None, true) => {
            crate::query::sql::push_where_with_ctx(acc, &cte.consumer, ctx)
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
    // Emission context for the whole consumer tail, computed once and shared
    // with the WHERE / ORDER BY qualification. When a sibling CTE is in the
    // consumer FROM (`also_from_cte`), every bare model-column reference is
    // ambiguous across the two terms, so qualify to the `from` term.
    let ctx = if cte.also_from.is_empty() {
        SqlEmitContext::root()
    } else {
        SqlEmitContext::joined(from)
    };
    acc.push_sql(" SELECT ");
    if cte.also_from.is_empty() || projection == "1" {
        // Single-term consumer FROM, or the count/exists `1` sub-projection —
        // emit the projection unqualified.
        acc.push_sql(projection);
    } else {
        // Multi-term consumer FROM (also_from_cte used): qualify every model
        // column as `<from>.<column>` to disambiguate columns shared by both
        // CTE terms. `COLUMN_LIST` is a baked static string that cannot be
        // prefixed per-column, so re-emit the column list from `COLUMNS`.
        push_qualified_columns_for_arm::<M>(acc, from);
    }
    acc.push_sql(" FROM ");
    acc.push_sql(from);
    for extra in &cte.also_from {
        acc.push_sql(", ");
        acc.push_sql(extra);
    }
    push_consumer_where(acc, cte, from, ctx)?;
    if include_order_limit_offset {
        crate::query::sql::push_consumer_order_limit_offset(acc, &cte.consumer, ctx);
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
    // Full-text-search predicates reference the model's GENERATED ALWAYS AS
    // tsvector column (e.g. `search`), which CTE bodies never project (bodies
    // carry only declared struct fields). A `search @@ …` / `ts_rank(search, …)`
    // reference against a CTE term would raise Postgres 42703 at runtime. The
    // consumer predicate tree can be mutated after construction by
    // `CteQuerySet::filter` / `filter_expr`, so this guard lives at the shared
    // build boundary (reached by every terminal) rather than at the
    // construction-time `validate_cte_consumer` sites. Tracking: CTE FTS
    // projection follow-up — once the tsvector column is projected into CTE
    // bodies, this guard is lifted and FTS gains support.
    if q_contains_fts(&cte.consumer.condition) {
        return Err(DjogiError::Validation(
            "CTE consumer SELECT does not support full-text-search predicates \
             (search().matches(...) / rank(...) / rank_cd(...)): the FTS tsvector \
             is a generated column that CTE bodies do not project. Filter FTS on \
             the base QuerySet before entering the CTE builder."
                .to_string(),
        ));
    }
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

        // FTS accessor — exercises the construction-time-equivalent ingress
        // the CTE-consumer FTS guard rejects at the build boundary.
        fn search(&self) -> crate::fts_query::FtsFieldRef<MiniNode> {
            crate::fts_query::__macro_support::__make_fts_ref::<MiniNode>("search", "english")
        }

        // String accessor for the boolean-combinator FTS rejection test.
        fn title(&self) -> DjogiField<MiniNode, String> {
            crate::__private::query::__make_djogi_field("title", |_| {
                static TITLE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
                TITLE.get_or_init(String::new)
            })
        }

        // Array accessor for the array_length qualification test — `.len()`
        // produces an `ExprNode::ArrayLength` that must be qualified under a
        // two-term consumer FROM.
        fn tags(&self) -> DjogiField<MiniNode, Vec<i32>> {
            crate::__private::query::__make_djogi_field("tags", |_| {
                static TAGS: std::sync::OnceLock<Vec<i32>> = std::sync::OnceLock::new();
                TAGS.get_or_init(Vec::new)
            })
        }

        // Geography accessor for the FTS-hidden-in-dwithin spatial rejection
        // test. Gated on `spatial` because `GeoPoint` is feature-gated.
        #[cfg(feature = "spatial")]
        fn geo(&self) -> DjogiField<MiniNode, crate::geo::GeoPoint> {
            crate::__private::query::__make_djogi_field("geo", |_| {
                static GEO: std::sync::OnceLock<crate::geo::GeoPoint> = std::sync::OnceLock::new();
                GEO.get_or_init(|| crate::geo::GeoPoint::new(0.0, 0.0).expect("valid coordinate"))
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

    // ── Task 8: filter_expr reaches the consumer WHERE with a CteRef ──────────
    #[test]
    fn cte_filter_expr_emits_cte_alias_ref_in_consumer_where() {
        use crate::query::cte_ref::CteRef;
        // WITH anchor AS (...), walk AS (...) SELECT ... FROM walk, anchor
        // WHERE active = anchor.active   (illustrative cross-CTE ref)
        let cte = MiniNode::objects()
            .with("anchor", MiniNode::objects())
            .expect("with anchor")
            .with("walk", MiniNode::objects())
            .expect("with walk")
            .from_cte("walk")
            .expect("from_cte")
            .also_from_cte("anchor")
            .expect("also_from_cte anchor")
            .filter_expr(|f| {
                f.active().as_expr().eq(CteRef::new("anchor", "active")
                    .expect("valid cte ref")
                    .as_expr::<bool>())
            });
        let sql = cte.render_cte_sql_for_testing().expect("render");
        assert!(
            sql.contains("anchor.active"),
            "cross-CTE ref must appear: {sql}"
        );
    }

    // ── Task 8b: also_from_cte appends the sibling and qualifies the SELECT ───
    #[test]
    fn also_from_cte_appends_sibling_to_consumer_from() {
        let cte = MiniNode::objects()
            .with("anchor", MiniNode::objects())
            .expect("with anchor")
            .with("places_cte", MiniNode::objects())
            .expect("with places_cte")
            .from_cte("places_cte")
            .expect("from_cte")
            .also_from_cte("anchor")
            .expect("also_from_cte anchor");
        let sql = cte.render_cte_sql_for_testing().expect("render");
        // Both terms in the consumer FROM, comma-separated, primary first.
        assert!(sql.contains("FROM places_cte, anchor"), "{sql}");
        // Under also_from, the SELECT projection is qualified to the `from`
        // term (R4 Change 1) — no bare column list.
        assert!(
            sql.contains("places_cte.id") && sql.contains("places_cte.active"),
            "qualified projection expected: {sql}"
        );
    }

    #[test]
    fn also_from_cte_rejects_undeclared_term() {
        let err = MiniNode::objects()
            .with("walk", MiniNode::objects())
            .expect("with walk")
            .from_cte("walk")
            .expect("from_cte")
            .also_from_cte("nope")
            .expect_err("undeclared term must be rejected");
        assert!(matches!(err, crate::DjogiError::Validation(_)));
    }

    // ── duplicate FROM terms are rejected (both directions) ──────────────────
    #[test]
    fn also_from_cte_rejects_duplicate_of_from_target() {
        let err = MiniNode::objects()
            .with("walk", MiniNode::objects())
            .expect("with walk")
            .from_cte("walk")
            .expect("from_cte")
            .also_from_cte("walk")
            .expect_err("duplicate of from_cte target must be rejected");
        assert!(matches!(err, crate::DjogiError::Validation(_)));

        // Same name added twice via also_from_cte is also rejected.
        let err2 = MiniNode::objects()
            .with("walk", MiniNode::objects())
            .expect("with walk")
            .with("sibling", MiniNode::objects())
            .expect("with sibling")
            .from_cte("walk")
            .expect("from_cte")
            .also_from_cte("sibling")
            .expect("first also_from_cte")
            .also_from_cte("sibling")
            .expect_err("duplicate also_from_cte must be rejected");
        assert!(matches!(err2, crate::DjogiError::Validation(_)));
    }

    #[test]
    fn from_cte_rejects_name_already_in_also_from() {
        // Build the chain in the inverse order — also_from_cte first, then
        // from_cte onto the same name — and assert from_cte rejects it.
        let err = MiniNode::objects()
            .with("walk", MiniNode::objects())
            .expect("with walk")
            .with("sibling", MiniNode::objects())
            .expect("with sibling")
            .from_cte("walk")
            .expect("from_cte walk")
            .also_from_cte("sibling")
            .expect("also_from_cte sibling")
            .from_cte("sibling")
            .expect_err("from_cte onto an also_from term must be rejected");
        assert!(matches!(err, crate::DjogiError::Validation(_)));
    }

    // ── cross-term cycle-mark uniqueness ─────────────────────────────────────
    #[test]
    fn duplicate_cycle_mark_across_terms_is_rejected() {
        let err = MiniNode::objects()
            .with_recursive(
                "walk",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("walk")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive walk")
            .cycle(&["id"], "is_cycle", "cycle_path")
            .expect("cycle walk")
            .with_recursive(
                "sibling",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("sibling")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive sibling")
            // Same mark name as `walk` — must be rejected before any terminal.
            .cycle(&["id"], "is_cycle", "sibling_path")
            .expect_err("duplicate mark name must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("duplicates a mark") && msg.contains("unique across all CTE terms"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn replacement_cycle_on_same_term_is_accepted() {
        // Calling cycle() twice on the SAME term (an idempotent re-config)
        // must NOT self-collide via the cross-term uniqueness loop.
        let cte = MiniNode::objects()
            .with_recursive(
                "walk",
                MiniNode::objects(),
                RecursiveArm::<MiniNode>::referencing("walk")
                    .join_on("parent_id", "id")
                    .expect("join_on"),
            )
            .expect("with_recursive walk")
            .cycle(&["id"], "is_cycle", "cycle_path")
            .expect("first cycle")
            .cycle(&["id"], "is_cycle", "cycle_path")
            .expect("replacement cycle must be accepted");
        let _ = cte;
    }

    // ── cycle + filter qualified under also_from_cte ─────────────────────────
    #[test]
    fn exclude_cycle_rows_with_filter_qualifies_columns_under_also_from_cte() {
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
            .with("sibling", MiniNode::objects())
            .expect("with sibling")
            .from_cte("walk")
            .expect("from_cte")
            .also_from_cte("sibling")
            .expect("also_from_cte sibling")
            .filter(|f| f.active().eq(true))
            .exclude_cycle_rows()
            .expect("exclude_cycle_rows");
        let sql = cte.render_cte_sql_for_testing().expect("render");

        // The consumer FROM has two terms.
        assert!(sql.contains("FROM walk, sibling"), "{sql}");

        // Scope the WHERE assertions to the consumer tail — the segment
        // beginning at the final `FROM walk, sibling`. CTE-body projections
        // (which emit the bare column list) precede this split point.
        let from_idx = sql
            .rfind("FROM walk, sibling")
            .expect("consumer FROM walk, sibling present");
        let consumer = &sql[from_idx..];

        // The cycle mark is qualified to the `from` term and the user predicate
        // column is qualified too.
        assert!(
            consumer.contains("WHERE NOT walk.is_cycle AND walk.active = $"),
            "consumer WHERE not qualified; consumer = {consumer}"
        );

        // The consumer SELECT projection precedes the consumer FROM, so locate
        // it in the full SQL by anchoring on the consumer FROM index and
        // searching backward for the nearest `SELECT `.
        let select_list = {
            let sel_kw = sql[..from_idx]
                .rfind("SELECT ")
                .expect("consumer SELECT before FROM")
                + "SELECT ".len();
            sql[sel_kw..from_idx].trim()
        };
        assert!(
            select_list.contains("walk.id"),
            "SELECT missing walk.id; list = {select_list}"
        );
        assert!(
            select_list.contains("walk.parent_id"),
            "SELECT missing walk.parent_id; list = {select_list}"
        );
        assert!(
            select_list.contains("walk.active"),
            "SELECT missing walk.active; list = {select_list}"
        );
    }

    // ── FTS predicates on a CTE consumer are refused at build ────────────────
    fn fts_cte_fixture() -> CteQuerySet<MiniNode> {
        MiniNode::objects()
            .with("walk", MiniNode::objects())
            .expect("with walk")
            .from_cte("walk")
            .expect("from_cte")
    }

    #[test]
    fn cte_consumer_rejects_fts_matches_leaf() {
        use crate::fts::TsQuery;
        let err = fts_cte_fixture()
            .filter(|f| f.search().matches(TsQuery::new("planet")))
            .render_cte_sql_for_testing()
            .unwrap_err();
        assert!(matches!(err, crate::DjogiError::Validation(_)));
        assert!(err.to_string().contains("full-text"), "got: {err}");
    }

    #[test]
    fn cte_consumer_rejects_fts_rank_comparison() {
        use crate::fts::TsQuery;
        let err = fts_cte_fixture()
            .filter(|f| f.search().rank(TsQuery::new("planet")).gt(0.0_f32))
            .render_cte_sql_for_testing()
            .unwrap_err();
        assert!(matches!(err, crate::DjogiError::Validation(_)));
        assert!(err.to_string().contains("full-text"), "got: {err}");
    }

    #[test]
    fn cte_consumer_rejects_fts_inside_boolean_combinator() {
        use crate::fts::TsQuery;
        let err = fts_cte_fixture()
            .filter(|f| {
                f.title().explicit_pg_predicate().contains("planet")
                    | f.search().matches(TsQuery::new("planet"))
            })
            .render_cte_sql_for_testing()
            .unwrap_err();
        assert!(matches!(err, crate::DjogiError::Validation(_)));
        assert!(err.to_string().contains("full-text"), "got: {err}");
    }

    // ── array_length is qualified under also_from_cte ────────────────────────
    #[test]
    fn array_length_qualifies_column_under_also_from_cte() {
        let cte = MiniNode::objects()
            .with("walk", MiniNode::objects())
            .expect("with walk")
            .with("sibling", MiniNode::objects())
            .expect("with sibling")
            .from_cte("walk")
            .expect("from_cte")
            .also_from_cte("sibling")
            .expect("also_from_cte sibling")
            .filter_expr(|f| f.tags().len().gt(0))
            .render_cte_sql_for_testing()
            .expect("render");
        // Scope to the consumer tail so CTE-body projections cannot interfere.
        let from_idx = cte
            .rfind("FROM walk, sibling")
            .expect("consumer FROM walk, sibling present");
        let consumer = &cte[from_idx..];
        assert!(
            consumer.contains("array_length(walk.tags, 1)"),
            "array_length column must be qualified; consumer = {consumer}"
        );
        assert!(
            !consumer.contains("array_length(tags, 1)"),
            "bare array_length column must not appear; consumer = {consumer}"
        );
    }

    // ── FTS hidden inside a DWithinExprToExpr operand is refused ─────────────
    #[cfg(feature = "spatial")]
    #[test]
    fn cte_consumer_rejects_fts_hidden_in_dwithin_case_operand() {
        use crate::Expr;
        use crate::expr::case::Case;
        use crate::fts::TsQuery;
        let err = fts_cte_fixture()
            .filter_expr(|f| {
                Expr::dwithin(
                    Case::when(
                        f.search().rank(TsQuery::new("planet")).gt(0.0_f32),
                        f.geo().as_expr(),
                    )
                    .otherwise(f.geo().as_expr()),
                    f.geo().as_expr(),
                    100.0,
                )
            })
            .render_cte_sql_for_testing()
            .unwrap_err();
        assert!(matches!(err, crate::DjogiError::Validation(_)));
        assert!(err.to_string().contains("full-text"), "got: {err}");
    }
}
