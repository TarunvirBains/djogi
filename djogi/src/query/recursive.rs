//! `RecursiveQuerySet<T>` — typed recursive-CTE query builder for
//! tree-shaped models. Phase 8-Zero Cluster B2 (T9 + T10 + T11 + T11b).
//!
//! # What
//!
//! A `RecursiveQuerySet<T>` is the entry point for walking a self-referential
//! parent edge on a model `T`. It is constructed via
//! [`QuerySet::tree_descendants`](crate::query::QuerySet::tree_descendants) /
//! [`QuerySet::tree_ancestors`](crate::query::QuerySet::tree_ancestors), or
//! via the inherent sugar [`Model::tree_descendants`] /
//! [`Model::tree_ancestors`] when the model declares
//! `#[model(tree_edge = "<column>")]`.
//!
//! Like [`QuerySet`](crate::query::QuerySet), it is lazy — every builder
//! method consumes `self` and returns `Self`; nothing reaches the database
//! until a terminal method ([`fetch_all`](RecursiveQuerySet::fetch_all),
//! [`count`](RecursiveQuerySet::count), [`exists`](RecursiveQuerySet::exists),
//! [`first`](RecursiveQuerySet::first)) is called.
//!
//! # Why a separate type (not extra `QuerySet` methods)
//!
//! Recursive queries compose differently from plain `SELECT`s — `offset`,
//! `distinct`, row locks, prefetch, and update / delete each require
//! recursive-CTE-specific handling that is either incorrect or actively
//! misleading on the regular `QuerySet`. Splitting the surface keeps the
//! plain queryset's API stable and cleanly excludes the methods that
//! cannot soundly compose with a recursive walk:
//!
//! - `offset` — `OFFSET n` over a recursive walk silently drops ancestors
//!   from the head of the result, which almost never matches caller
//!   intent.
//! - `distinct` / `distinct_on` — already handled by the `CYCLE id` clause
//!   the SQL builder always emits; a second DISTINCT on top would be both
//!   redundant and prone to suppressing legitimately-distinct rows.
//! - `select_for_update` / `nowait` / `skip_locked` — row locks on a
//!   recursive walk acquire one lock per visited row in walk order. Pre-
//!   1.0 we ban this until we have a clear "lock the whole subtree
//!   atomically" story (out of scope for Phase 8-Zero).
//! - `prefetch` / `select_related` — fan-out over a tree multiplies the
//!   round trips by the size of the subtree; the right shape is a single
//!   joined recursive CTE the user expresses directly. Banning the wrong
//!   shape avoids accidental N+1 over a tree.
//! - bulk `update` / `delete` — non-trivial cascade semantics; deferred
//!   to a later phase when we can declare the lock and visibility model.
//!
//! # SQL shape
//!
//! The emitter produces one of:
//!
//! ```sql
//! -- DESCENDANTS
//! WITH RECURSIVE __djogi_tree (depth, <cols...>) AS (
//!     SELECT 0, <cols...>
//!     FROM <table>
//!     WHERE id = $1
//!   UNION ALL
//!     SELECT parent.depth + 1, <child.cols...>
//!     FROM <table> child
//!     JOIN __djogi_tree parent ON child.<edge_col> = parent.id
//!     [WHERE <user_filter>]
//!     [AND parent.depth < $n]
//! ) [SEARCH BREADTH FIRST BY <col> SET _djogi_search_seq]
//!   CYCLE id SET is_cycle USING path
//! SELECT <cols...> FROM __djogi_tree
//! WHERE NOT is_cycle
//! [ORDER BY <_djogi_search_seq,> <user_order>]
//! ```
//!
//! For `tree_ancestors` the join condition flips to
//! `parent.<edge_col> = child.id` — child walks up, parent has the FK
//! pointing at child.
//!
//! # SQL invariants
//!
//! - **`UNION ALL`**, never `UNION` — multiplicity preservation matters
//!   for B3's `full_ancestors` Wright-correctness path; using `ALL`
//!   keeps the codepath identical even though B2 only does single-edge
//!   walks.
//! - **`CYCLE id SET is_cycle USING path`** is mandatory. Postgres
//!   manages both `is_cycle` and `path` automatically — they do not
//!   appear in our manual column list. The outer `WHERE NOT is_cycle`
//!   strips the cycle-detection sentinel rows from output.
//! - **`SEARCH ... BY <col> SET _djogi_search_seq`** emits only when
//!   the caller invoked
//!   [`search_breadth_first_by`](RecursiveQuerySet::search_breadth_first_by) /
//!   [`search_depth_first_by`](RecursiveQuerySet::search_depth_first_by).
//!   The internal sequence column `_djogi_search_seq` is macro-internal
//!   (`_djogi_*` prefix is forbidden as a user column name by the
//!   identifier validator) so it cannot collide with model fields. It is
//!   never projected into the outer SELECT, but the outer `ORDER BY`
//!   references it so callers see BFS / DFS order without an explicit
//!   `order_by` call.
//! - **RLS:** every terminal calls
//!   [`auto_set_tenant`](crate::query::terminal::auto_set_tenant) before
//!   building SQL, exactly like the plain `QuerySet` terminals. Without
//!   this, recursive walks leak across tenants whenever the model carries
//!   a `tenant_key`.
//!
//! # `clippy::manual_async_fn`
//!
//! Every terminal returns `impl Future<Output = ...> + Send + 'ctx`
//! rather than `async fn`. The explicit-bound form matches the
//! [`Model`](crate::model::Model) trait's RPITIT shape and is required
//! so the returned futures are `Send` for use under multi-threaded
//! Tokio runtimes (e.g. inside an Axum handler). The lint fires on
//! every such method; allowing it at the module level matches the
//! same allowance in [`crate::query::terminal`].
#![allow(clippy::manual_async_fn)]

use crate::DjogiError;
use crate::context::DjogiContext;
use crate::model::Model;
use crate::pg::accumulator::{SqlAccumulator, as_params};
use crate::pg::decode::{FromPgRow, try_get_scalar};
use crate::query::condition::Condition;
use crate::query::field::FieldRef;
use crate::query::order::OrderExpr;
use crate::query::sql::emit_condition;
use crate::query::terminal::auto_set_tenant;
use crate::relation::path::{RelationKind, RelationPath};
use postgres_types::ToSql;
use std::future::Future;
use std::marker::PhantomData;

/// Direction of the recursive walk.
///
/// `Descendants` walks downward — given a root row, accumulate every row
/// whose self-FK chain leads back up to the root. `Ancestors` walks
/// upward — given a leaf, accumulate every row reached by following the
/// self-FK from the current node to its parent.
///
/// Stored on the builder; consulted by the SQL emitter to pick the
/// recursive-term JOIN condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecursiveDirection {
    /// Walk children: `child.<edge_col> = parent.id`.
    Descendants,
    /// Walk parents: `parent.<edge_col> = child.id`.
    Ancestors,
}

/// Search-order discriminator.
///
/// Mutually exclusive with itself — last call to
/// [`search_breadth_first_by`](RecursiveQuerySet::search_breadth_first_by) /
/// [`search_depth_first_by`](RecursiveQuerySet::search_depth_first_by)
/// wins. Type-state would be over-engineered for v0.1.0; mutual exclusion
/// is documented behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchMode {
    /// `SEARCH BREADTH FIRST BY <col> SET _djogi_search_seq`.
    Breadth(&'static str),
    /// `SEARCH DEPTH FIRST BY <col> SET _djogi_search_seq`.
    Depth(&'static str),
}

impl SearchMode {
    fn keyword(self) -> &'static str {
        match self {
            SearchMode::Breadth(_) => " SEARCH BREADTH FIRST BY ",
            SearchMode::Depth(_) => " SEARCH DEPTH FIRST BY ",
        }
    }
    fn column(self) -> &'static str {
        match self {
            SearchMode::Breadth(c) | SearchMode::Depth(c) => c,
        }
    }
}

/// Lazy recursive-CTE query builder. Nothing hits the database until a
/// terminal method is called.
///
/// Constructed via
/// [`QuerySet::tree_descendants`](crate::query::QuerySet::tree_descendants) /
/// [`QuerySet::tree_ancestors`](crate::query::QuerySet::tree_ancestors)
/// (typed-path form, works on any model with at least one self-FK), or via
/// the inherent sugar
/// [`Model::tree_descendants`] / [`Model::tree_ancestors`] (works only on
/// models that declare `#[model(tree_edge = "...")]`).
///
/// See the module-level documentation for the SQL shape, the `UNION ALL`
/// rationale, and the auto-tenant contract.
pub struct RecursiveQuerySet<T: Model> {
    pub(crate) direction: RecursiveDirection,
    /// Self-FK column on `T`'s table — `&'static str` from
    /// `RelationPath::source_column()`. Identifier-validated at the
    /// macro-emission site (`__make_relation_path`), so direct
    /// `push_sql` is safe.
    pub(crate) edge_column: &'static str,
    /// Root identifier — the row the walk starts from. Boxed `dyn ToSql`
    /// so the field type is independent of `T::Pk`'s concrete shape;
    /// the builder methods are themselves generic over `T::Pk`.
    pub(crate) root_id: Box<dyn ToSql + Sync + Send>,
    /// Accumulated user filter — AND-ed onto the recursive term's
    /// `WHERE`. Anchor (root) row is *not* filtered through this; the
    /// anchor's only condition is `id = $1`.
    pub(crate) condition: Condition,
    /// Outer `ORDER BY` clauses — applied to the materialised CTE,
    /// never inside it. SEARCH BFS/DFS, when set, prepends an
    /// implicit `_djogi_search_seq` term so the user's ordering
    /// becomes a tiebreaker after the search-order key.
    pub(crate) ordering: Vec<OrderExpr>,
    /// Optional recursive-depth cap. `None` means unbounded — only the
    /// `CYCLE id` clause's cycle detection bounds termination.
    pub(crate) max_depth: Option<u32>,
    /// SEARCH BFS/DFS state — mutually exclusive within itself.
    pub(crate) search_mode: Option<SearchMode>,
    _model: PhantomData<fn() -> T>,
}

impl<T: Model> std::fmt::Debug for RecursiveQuerySet<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecursiveQuerySet")
            .field("table", &T::table_name())
            .field("direction", &self.direction)
            .field("edge_column", &self.edge_column)
            .field("condition", &self.condition)
            .field("ordering", &self.ordering)
            .field("max_depth", &self.max_depth)
            .field("search_mode", &self.search_mode)
            .finish_non_exhaustive()
    }
}

impl<T: Model> RecursiveQuerySet<T> {
    /// Construct from a typed [`RelationPath<T, T>`] and a root id —
    /// the work-horse constructor that
    /// [`QuerySet::tree_descendants`](crate::query::QuerySet::tree_descendants) /
    /// [`QuerySet::tree_ancestors`](crate::query::QuerySet::tree_ancestors)
    /// delegate into. Crate-private — callers reach through the typed
    /// `QuerySet` methods.
    ///
    /// Validates only the relation kind: a self-FK relation must be
    /// `RelationKind::ForeignKey` or `RelationKind::OneToOne`. Anything
    /// else (`ManyToMany`, future variants) cannot anchor a tree walk.
    /// `RelationPath<T, T>` already pins source == target at the type
    /// level via the macro-emitted `__make_relation_path`, so the kind
    /// check here is the only remaining runtime validation.
    pub(crate) fn from_path(
        path: RelationPath<T, T>,
        root_id: T::Pk,
        direction: RecursiveDirection,
    ) -> Self
    where
        T::Pk: ToSql + Sync + Send + 'static,
    {
        // ManyToMany self-relations are syntactically expressible but
        // structurally incompatible with the recursive-CTE shape — they
        // would need a JOIN through the through-model, which is a
        // different SQL pattern entirely. Reject at builder time so the
        // wrong call site fails fast rather than at SQL execution.
        debug_assert!(
            matches!(
                path.kind(),
                RelationKind::ForeignKey | RelationKind::OneToOne
            ),
            "RecursiveQuerySet requires a ForeignKey or OneToOne self-FK; got {:?}",
            path.kind()
        );

        Self {
            direction,
            edge_column: path.source_column(),
            root_id: Box::new(root_id),
            condition: Condition::True,
            ordering: Vec::new(),
            max_depth: None,
            search_mode: None,
            _model: PhantomData,
        }
    }

    /// AND a typed filter closure onto the recursive-term `WHERE`.
    ///
    /// Same closure shape as [`QuerySet::filter`](crate::query::QuerySet::filter)
    /// — receives a default-constructed `T::Fields` and returns a
    /// [`Condition`]. The predicate applies to **every recursive step**;
    /// the anchor row (the root) is matched only by `id = $1`, never
    /// through this filter. This matches caller intent: "give me the
    /// subtree rooted here, narrowed by predicate" — narrowing the root
    /// would change which subtree is being walked, not which rows in it
    /// match.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn filter<F>(mut self, f: F) -> Self
    where
        F: FnOnce(T::Fields) -> Condition,
    {
        let cond = f(T::Fields::default());
        self.condition = Condition::and(self.condition, cond);
        self
    }

    /// AND an expression-IR predicate onto the recursive-term `WHERE`.
    ///
    /// Field-vs-field comparisons and arithmetic predicates work exactly
    /// as on [`QuerySet::filter_expr`](crate::query::QuerySet::filter_expr) —
    /// the closure returns an `Expr<bool>`, which is wrapped in
    /// [`Condition::Expr`] before being AND-ed onto the accumulated tree.
    /// Same anchor-row caveat as [`filter`](Self::filter): the anchor is
    /// matched only by `id = $1`.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn filter_expr<F>(mut self, f: F) -> Self
    where
        F: FnOnce(T::Fields) -> crate::expr::Expr<bool>,
    {
        let expr = f(T::Fields::default());
        self.condition = Condition::and(self.condition, Condition::Expr(expr));
        self
    }

    /// Append outer `ORDER BY` clauses applied **after** CTE
    /// materialization.
    ///
    /// Ordering is never injected inside the recursive term — it would
    /// have no defined effect on `UNION ALL` and Postgres's recursive
    /// query planner explicitly disallows it. Use
    /// [`search_breadth_first_by`](Self::search_breadth_first_by) /
    /// [`search_depth_first_by`](Self::search_depth_first_by) when the
    /// goal is BFS / DFS traversal order, not lexical column ordering.
    ///
    /// Multiple `order_by` calls **append** in Django-style — library
    /// code can stack tiebreakers without clobbering the caller's
    /// primary ordering.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn order_by<F, O>(mut self, f: F) -> Self
    where
        F: FnOnce(T::Fields) -> O,
        O: Into<Vec<OrderExpr>>,
    {
        let exprs: Vec<OrderExpr> = f(T::Fields::default()).into();
        self.ordering.extend(exprs);
        self
    }

    /// Bound recursive depth — emits `AND parent.depth < $n` in the
    /// recursive term.
    ///
    /// **No default.** When this method is not called, the walk runs to
    /// natural exhaustion or until the `CYCLE id` clause detects a cycle.
    /// Both termination paths are correct; pick `with_max_depth` only
    /// when caller intent really is "stop after N hops" (e.g. UI
    /// breadcrumb that should never render more than 5 ancestors).
    ///
    /// The bound is bound as a positional `$n` parameter — the value
    /// never appears verbatim in the emitted SQL.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn with_max_depth(mut self, n: u32) -> Self {
        self.max_depth = Some(n);
        self
    }

    /// Emit `SEARCH BREADTH FIRST BY <col> SET _djogi_search_seq` on
    /// the CTE.
    ///
    /// Postgres annotates each recursive row with a sequence value the
    /// outer SELECT's `ORDER BY _djogi_search_seq` then sorts by; this
    /// terminal automatically prepends that ORDER BY (the user's
    /// `order_by` clauses, if any, append after as tiebreakers) so
    /// callers see BFS-traversal order without writing the order term
    /// by hand.
    ///
    /// Mutually exclusive with [`search_depth_first_by`](Self::search_depth_first_by) —
    /// last call wins. Type-state would be heavy for v0.1.0; mutual
    /// exclusion is documented behaviour.
    ///
    /// `_djogi_search_seq` is macro-internal — the underscore prefix
    /// blocks identifier validation from accepting it as a user column
    /// name, so a model field cannot collide with the synthetic search
    /// column.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn search_breadth_first_by<V>(mut self, field: FieldRef<T, V>) -> Self {
        self.search_mode = Some(SearchMode::Breadth(field.column()));
        self
    }

    /// Emit `SEARCH DEPTH FIRST BY <col> SET _djogi_search_seq` on
    /// the CTE — DFS sibling of
    /// [`search_breadth_first_by`](Self::search_breadth_first_by).
    ///
    /// Same auto-prepended outer `ORDER BY _djogi_search_seq`,
    /// same mutual-exclusion rule.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn search_depth_first_by<V>(mut self, field: FieldRef<T, V>) -> Self {
        self.search_mode = Some(SearchMode::Depth(field.column()));
        self
    }
}

// ── SQL builder ────────────────────────────────────────────────────────────

/// Emit the canonical column list with a per-column `<alias>.` prefix —
/// `<alias>.col1, <alias>.col2, ...`.
///
/// Used inside the CTE's anchor and recursive terms to project every
/// column of `T` from a specific table reference (`<table>` for the
/// anchor, `child` for the recursive term). The bare-name form
/// (`<T as FromPgRow>::COLUMN_LIST`) is reused unchanged for the outer
/// SELECT, which reads from the CTE alias `__djogi_tree` and needs no
/// per-column prefix. Each column name is `&'static str` macro-baked
/// via [`crate::ident::assert_plain_ident`], so direct `push_sql` is
/// safe.
fn push_qualified_columns<T: FromPgRow>(acc: &mut SqlAccumulator, alias: &'static str) {
    for (i, col) in <T as FromPgRow>::COLUMNS.iter().enumerate() {
        if i > 0 {
            acc.push_sql(", ");
        }
        acc.push_sql(alias);
        acc.push_sql(".");
        acc.push_sql(col);
    }
}

/// Build the recursive-CTE SELECT for `qs`. Pure SQL emission — never
/// touches a connection. The returned [`SqlAccumulator`] is consumed by
/// the terminals below.
///
/// Consumes the queryset because the root id is stored as a
/// `Box<dyn ToSql + Sync + Send>` that the emitter moves into the
/// accumulator's bind vector. Terminals already take `self` by value,
/// so the consume-by-value shape composes naturally.
///
/// Bind ordering: `$1` is the root id; subsequent `$n` slots are
/// allocated by [`emit_condition`] for the user filter and by
/// [`with_max_depth`](RecursiveQuerySet::with_max_depth) for the depth
/// cap. `tokio_postgres::Row` gets the values back in the order
/// `acc.into_parts().1` returns them.
pub(crate) fn build_recursive_select<T: Model + FromPgRow>(
    qs: RecursiveQuerySet<T>,
) -> SqlAccumulator {
    build_recursive_inner::<T>(qs, RecursiveProjection::Rows)
}

/// `SELECT COUNT(*) FROM (...)` over the same CTE — wraps the row form
/// in a subquery so cycle stripping and the SEARCH ORDER BY (ignored
/// for COUNT but harmless) survive the count rewrite.
pub(crate) fn build_recursive_count<T: Model + FromPgRow>(
    qs: RecursiveQuerySet<T>,
) -> SqlAccumulator {
    build_recursive_inner::<T>(qs, RecursiveProjection::Count)
}

/// `SELECT EXISTS(SELECT 1 FROM (...) LIMIT 1)` — same wrap as count
/// but optimised for the early-exit semantics of EXISTS.
pub(crate) fn build_recursive_exists<T: Model + FromPgRow>(
    qs: RecursiveQuerySet<T>,
) -> SqlAccumulator {
    build_recursive_inner::<T>(qs, RecursiveProjection::Exists)
}

/// Outer projection mode for the shared recursive-CTE emitter.
///
/// Three terminals share the recursive-CTE shape (anchor + recursive
/// term + CYCLE + optional SEARCH); only the outer SELECT differs.
/// Routing through this enum keeps the CTE definition emitted exactly
/// once across `fetch_all` / `count` / `exists`, avoiding the
/// drift-by-copy hazard a three-way function split would introduce.
#[derive(Debug, Clone, Copy)]
enum RecursiveProjection {
    /// Outer `SELECT <cols...>` — the row terminal.
    Rows,
    /// Outer `SELECT COUNT(*)` — wraps the row form in a subquery.
    Count,
    /// Outer `SELECT EXISTS (... LIMIT 1)` — wraps the row form.
    Exists,
}

fn build_recursive_inner<T: Model + FromPgRow>(
    qs: RecursiveQuerySet<T>,
    projection: RecursiveProjection,
) -> SqlAccumulator {
    let mut acc = SqlAccumulator::new("");

    // The Count / Exists wraps need an outer expression around the row
    // SELECT. Open the wrap before the WITH so bind ordering stays
    // stable (the WITH's binds are still $1.. counted from the start).
    match projection {
        RecursiveProjection::Rows => {}
        RecursiveProjection::Count => acc.push_sql("SELECT COUNT(*) FROM ("),
        RecursiveProjection::Exists => acc.push_sql("SELECT EXISTS ("),
    }

    // ── WITH RECURSIVE __djogi_tree (depth, <cols...>) AS ( ──────────────
    acc.push_sql("WITH RECURSIVE __djogi_tree (depth, ");
    acc.push_sql(<T as FromPgRow>::COLUMN_LIST);
    acc.push_sql(") AS (");

    // ── Anchor: SELECT 0, <cols...> FROM <table> WHERE id = $1 ───────────
    acc.push_sql("SELECT 0, ");
    push_qualified_columns::<T>(&mut acc, T::table_name());
    acc.push_sql(" FROM ");
    acc.push_sql(T::table_name());
    acc.push_sql(" WHERE ");
    acc.push_sql(T::table_name());
    acc.push_sql(".id = ");
    // Root id — bound as `$1`. The builder owns the value as
    // `Box<dyn ToSql + Sync + Send>`; we wrap it in a [`DynBind`]
    // newtype that delegates `ToSql` through the box's dyn-safe
    // `to_sql_checked`. One allocation per terminal, no unsafe.
    acc.push_bind(DynBind(qs.root_id));

    // ── UNION ALL — recursive term ───────────────────────────────────────
    //
    // `UNION ALL` (not `UNION`) is load-bearing for B3's full_ancestors
    // multiplicity-preservation path. Even though B2 only does single-
    // edge walks, using `ALL` here means the codepath stays identical
    // when B3 lands and avoids a silent semantic change at that boundary.
    acc.push_sql(" UNION ALL SELECT parent.depth + 1, ");
    push_qualified_columns::<T>(&mut acc, "child");
    acc.push_sql(" FROM ");
    acc.push_sql(T::table_name());
    acc.push_sql(" child JOIN __djogi_tree parent ON ");
    match qs.direction {
        // Descendants: walk down. Parent's id matches child's edge column.
        RecursiveDirection::Descendants => {
            acc.push_sql("child.");
            acc.push_sql(qs.edge_column);
            acc.push_sql(" = parent.id");
        }
        // Ancestors: walk up. Parent's edge column points at child's id.
        RecursiveDirection::Ancestors => {
            acc.push_sql("parent.");
            acc.push_sql(qs.edge_column);
            acc.push_sql(" = child.id");
        }
    }

    // Recursive-term WHERE — user filter and / or depth cap. We open
    // the WHERE only when at least one predicate fires so the emitted
    // SQL stays minimal in the common no-filter case.
    let has_user_filter = !qs.condition.is_vacuously_true();
    let has_depth_cap = qs.max_depth.is_some();
    if has_user_filter || has_depth_cap {
        acc.push_sql(" WHERE ");
        if has_user_filter {
            // The user filter references `T::Fields` columns, which
            // emit as bare names. The recursive term aliases the
            // model's table as `child` — qualifying the user's
            // predicate as `child.<col>` keeps Postgres from raising
            // `42702 column reference ambiguous` against the
            // `__djogi_tree parent` side of the JOIN, which exposes
            // the same column names through the same alias scope.
            emit_condition(&mut acc, qs.condition.clone(), Some("child"));
        }
        if has_depth_cap {
            if has_user_filter {
                acc.push_sql(" AND ");
            }
            acc.push_sql("parent.depth < ");
            // Bind `n` as `i64` — Postgres has no unsigned types and
            // the column type for `parent.depth` is INTEGER (driven by
            // `0` in the anchor). Going through `i64` is over-wide but
            // keeps the bind shape consistent across `with_max_depth`
            // call sites that may be plumbed `u32` from upstream
            // configuration.
            let n = qs.max_depth.expect("max_depth set above");
            acc.push_bind(n as i64);
        }
    }

    // ── ) [SEARCH ...] CYCLE id SET is_cycle USING path ──────────────────
    acc.push_sql(")");
    if let Some(mode) = qs.search_mode {
        acc.push_sql(mode.keyword());
        acc.push_sql(mode.column());
        acc.push_sql(" SET _djogi_search_seq");
    }
    // CYCLE: Postgres detects cycles using `path`, marks them in
    // `is_cycle`, and stops recursion at the marked row. Without this
    // clause, a malformed self-FK chain (cycle introduced by buggy
    // application code or a manual SQL edit) would loop forever. The
    // `id` column-name + the synthetic `path` / `is_cycle` columns are
    // managed entirely by Postgres — they do not appear in our column
    // list and cannot collide with user fields.
    acc.push_sql(" CYCLE id SET is_cycle USING path");

    // ── Outer SELECT ──────────────────────────────────────────────────────
    match projection {
        RecursiveProjection::Rows => {
            acc.push_sql(" SELECT ");
            acc.push_sql(<T as FromPgRow>::COLUMN_LIST);
            acc.push_sql(" FROM __djogi_tree WHERE NOT is_cycle");
            // ORDER BY: SEARCH-ordering first (so BFS / DFS works
            // without an explicit `order_by`), then the user's
            // ordering as tiebreakers. Either alone is valid; both
            // together is valid SQL and matches Django's
            // append-tiebreakers convention.
            let has_search_order = qs.search_mode.is_some();
            let has_user_order = !qs.ordering.is_empty();
            if has_search_order || has_user_order {
                acc.push_sql(" ORDER BY ");
                if has_search_order {
                    acc.push_sql("_djogi_search_seq");
                }
                if has_user_order {
                    if has_search_order {
                        acc.push_sql(", ");
                    }
                    for (i, o) in qs.ordering.iter().enumerate() {
                        if i > 0 {
                            acc.push_sql(", ");
                        }
                        // Outer SELECT runs against `__djogi_tree`,
                        // which exposes T's columns under their bare
                        // names — same shape as the plain `QuerySet`
                        // ordering emit, so `parent_table = None`.
                        o.emit(&mut acc, None);
                    }
                }
            }
        }
        RecursiveProjection::Count => {
            // Inner SELECT inside the COUNT subquery — projects only
            // `1` since we just need rows to count. ORDER BY would be
            // discarded by the COUNT wrap; SEARCH-ordering likewise
            // has no observable effect on COUNT, so we omit both for
            // a minimal-shape inner query.
            acc.push_sql(" SELECT 1 FROM __djogi_tree WHERE NOT is_cycle) AS sub");
        }
        RecursiveProjection::Exists => {
            // `EXISTS(... LIMIT 1)` — Postgres stops scanning at the
            // first match. Same minimal projection as the count path.
            acc.push_sql(" SELECT 1 FROM __djogi_tree WHERE NOT is_cycle LIMIT 1)");
        }
    }

    acc
}

// ── Terminals ──────────────────────────────────────────────────────────────

impl<T: Model> RecursiveQuerySet<T>
where
    T: FromPgRow + Send + Unpin,
{
    /// Materialise every reachable row into a `Vec<T>`.
    ///
    /// Honours the [`auto_set_tenant`] contract — RLS-keyed models have
    /// `app.tenant_id` set from the caller's auth context before the
    /// CTE runs. Order of returned rows depends on the builder state:
    /// SEARCH BFS / DFS (when set) ordered first, then any
    /// [`order_by`](Self::order_by) clauses; without either, Postgres
    /// returns CTE rows in storage order with no guarantee of
    /// hierarchical layout.
    pub fn fetch_all<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Vec<T>, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
    {
        async move {
            auto_set_tenant::<T>(ctx).await?;
            let acc = build_recursive_select(self);
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let rows = ctx.query_all(&sql, &params).await?;
            rows.iter()
                .map(|r| T::from_pg_row(r))
                .collect::<Result<Vec<T>, _>>()
        }
    }

    /// Return the first reachable row, or `None` if the walk yields
    /// no rows.
    ///
    /// Internally piggy-backs on [`fetch_all`](Self::fetch_all) +
    /// `take(1)` rather than a tailored `LIMIT 1` because applying
    /// `LIMIT` to the outer SELECT after the CYCLE / SEARCH machinery
    /// has run is the same physical work as fetching one row — the
    /// CTE materialises lazily under Postgres's recursive planner and
    /// stops as soon as the outer cursor is closed. Pre-1.0 we keep
    /// the implementation simple; if profiling shows the redundant
    /// allocation matters, a dedicated emitter is a one-line change.
    pub fn first<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Option<T>, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
    {
        async move {
            auto_set_tenant::<T>(ctx).await?;
            let acc = build_recursive_select(self);
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let opt = ctx.query_opt(&sql, &params).await?;
            opt.as_ref().map(|r| T::from_pg_row(r)).transpose()
        }
    }
}

impl<T: Model> RecursiveQuerySet<T>
where
    T: FromPgRow,
{
    /// `SELECT COUNT(*) FROM (... recursive CTE ...)` — the
    /// reachable-row count.
    ///
    /// `i64` to match Postgres's `BIGINT` `COUNT(*)` result and leave
    /// headroom for tables that grow past `i32::MAX`.
    pub fn count<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<i64, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
    {
        async move {
            auto_set_tenant::<T>(ctx).await?;
            let acc = build_recursive_count(self);
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let row = ctx.query_one(&sql, &params).await?;
            try_get_scalar::<i64>(&row, 0)
        }
    }

    /// `SELECT EXISTS(SELECT 1 FROM (... recursive CTE ...) LIMIT 1)` —
    /// "does the walk reach at least one row" without materialising the
    /// whole subtree.
    pub fn exists<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<bool, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
    {
        async move {
            auto_set_tenant::<T>(ctx).await?;
            let acc = build_recursive_exists(self);
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let row = ctx.query_one(&sql, &params).await?;
            try_get_scalar::<bool>(&row, 0)
        }
    }
}

// ── DynBind: type-erased ToSql carrier ─────────────────────────────────────

/// Newtype wrapping `Box<dyn ToSql + Sync + Send>` so it can satisfy
/// the `T: ToSql + Sync + Send + 'static` bound on
/// [`SqlAccumulator::push_bind`].
///
/// `Box<dyn ToSql + Sync + Send>` itself does not impl `ToSql` —
/// `ToSql` is not auto-implemented for trait objects because of its
/// associated `accepts` fn. Stamping a thin newtype with delegating
/// `to_sql` / `accepts` impls is the canonical workaround. Every
/// method threads through to the inner box's dyn-safe
/// [`postgres_types::ToSql::to_sql_checked`], so the value's encoding
/// is exactly what `T::Pk`'s native `ToSql` impl produces — no wire-
/// format reinterpretation, no unsafe, no extra allocation beyond the
/// `Box` already on the queryset.
///
/// `RecursiveQuerySet` carries the root id as `Box<dyn ToSql + Sync +
/// Send>` so the builder's struct shape is independent of `T::Pk`'s
/// concrete type. At terminal time the box moves into a [`DynBind`]
/// and that wrapper is handed to the accumulator's bind vector — one
/// allocation per terminal call, the same shape every other terminal
/// pays.
struct DynBind(Box<dyn ToSql + Sync + Send>);

impl std::fmt::Debug for DynBind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The inner box's `Debug` is not reachable through the trait
        // object (`ToSql: Debug` makes a *static* `Debug` impl
        // available on each concrete type, but `dyn ToSql` cannot
        // surface it without an additional `Debug` supertrait we
        // chose not to add). The wire-format representation in the
        // accumulator is the only useful debug surface anyway, and
        // accumulator-level logs render the bind ordinal — so a
        // placeholder is the right shape here.
        f.write_str("DynBind(<root_id>)")
    }
}

impl ToSql for DynBind {
    fn to_sql(
        &self,
        ty: &postgres_types::Type,
        out: &mut bytes::BytesMut,
    ) -> Result<postgres_types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        // Delegate to the inner box's dyn-safe `to_sql_checked`. The
        // `to_sql_checked` method is the official dyn-trait-safe
        // entry point — `to_sql` itself isn't dyn-safe because of
        // the type bound on the static `accepts` fn, but
        // `to_sql_checked` works against trait objects.
        self.0.to_sql_checked(ty, out)
    }

    fn accepts(_ty: &postgres_types::Type) -> bool {
        // Always accept — the inner box's `to_sql_checked` will fail
        // with a descriptive `WrongType` error if the runtime type
        // doesn't match. This keeps the type guard at the postgres-
        // types runtime layer (which has rich error context) rather
        // than duplicating it at the newtype layer.
        true
    }

    postgres_types::to_sql_checked!();
}

#[cfg(test)]
mod tests {
    //! SQL-builder unit tests — every assertion is a string check on
    //! the emitted SQL or a count of bind slots. No live database is
    //! reached here; integration tests against a real Postgres live
    //! in B5.
    //!
    //! `MiniTree` is a stub `Model` + `FromPgRow` impl just rich
    //! enough to drive the recursive-CTE emitter. Its CRUD methods
    //! panic — they're never called in these tests, which exercise
    //! only the SQL emission path.

    use super::*;
    use crate::descriptor::ModelDescriptor;
    use crate::pg::decode::FromPgRow;
    use crate::types::HeerId;

    struct MiniTree;

    impl crate::model::__sealed::Sealed for MiniTree {}

    impl crate::model::Model for MiniTree {
        type Pk = HeerId;
        type Fields = ();
        fn table_name() -> &'static str {
            "mini_trees"
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
    }

    impl FromPgRow for MiniTree {
        const COLUMNS: &'static [&'static str] = &["id", "parent_id", "label"];
        const COLUMN_LIST: &'static str = "id, parent_id, label";
        fn from_pg_row(_row: &tokio_postgres::Row) -> Result<Self, DjogiError> {
            unreachable!()
        }
    }

    fn root() -> RecursiveQuerySet<MiniTree> {
        RecursiveQuerySet::<MiniTree>::from_path(
            crate::relation::__macro_support::__make_relation_path::<MiniTree, MiniTree>(
                "parent_id",
                "mini_trees",
                RelationKind::ForeignKey,
            ),
            HeerId::from_i64(1).unwrap(),
            RecursiveDirection::Descendants,
        )
    }

    fn ancestors_root() -> RecursiveQuerySet<MiniTree> {
        RecursiveQuerySet::<MiniTree>::from_path(
            crate::relation::__macro_support::__make_relation_path::<MiniTree, MiniTree>(
                "parent_id",
                "mini_trees",
                RelationKind::ForeignKey,
            ),
            HeerId::from_i64(1).unwrap(),
            RecursiveDirection::Ancestors,
        )
    }

    #[test]
    fn descendants_emits_union_all_with_child_join() {
        let qs = root();
        let acc = build_recursive_select(qs);
        let sql = acc.sql();
        assert!(
            sql.contains("UNION ALL"),
            "recursive term must use UNION ALL, got: {sql}"
        );
        // Critically, never the bare `UNION` keyword — that would
        // dedup multiplicity and break B3's full_ancestors path.
        assert!(
            !sql.contains(" UNION SELECT"),
            "recursive term must not use plain UNION (multiplicity loss): {sql}"
        );
        assert!(
            sql.contains("child.parent_id = parent.id"),
            "descendants JOIN must walk child.parent_id = parent.id: {sql}"
        );
    }

    #[test]
    fn ancestors_flips_join_direction() {
        let qs = ancestors_root();
        let acc = build_recursive_select(qs);
        let sql = acc.sql();
        assert!(
            sql.contains("parent.parent_id = child.id"),
            "ancestors JOIN must walk parent.parent_id = child.id: {sql}"
        );
    }

    #[test]
    fn cycle_clause_is_always_emitted() {
        // CYCLE detection is mandatory — a malformed self-FK chain
        // must not loop forever even when the caller forgets
        // `with_max_depth`.
        let qs = root();
        let acc = build_recursive_select(qs);
        let sql = acc.sql();
        assert!(
            sql.contains("CYCLE id SET is_cycle USING path"),
            "CYCLE clause must always be emitted: {sql}"
        );
        assert!(
            sql.contains("WHERE NOT is_cycle"),
            "outer SELECT must filter cycle sentinel rows: {sql}"
        );
    }

    #[test]
    fn outer_projection_uses_canonical_column_list() {
        // The outer SELECT reads from `__djogi_tree`, which exposes
        // T's columns under their bare names — so projection is
        // simply `<T as FromPgRow>::COLUMN_LIST`. No `_djogi_*`
        // internal columns leak.
        let qs = root();
        let acc = build_recursive_select(qs);
        let sql = acc.sql();
        assert!(
            sql.contains(" SELECT id, parent_id, label FROM __djogi_tree"),
            "outer SELECT must project canonical columns: {sql}"
        );
        assert!(
            !sql.contains("depth FROM __djogi_tree"),
            "depth column must not leak into outer projection: {sql}"
        );
        assert!(
            !sql.contains("_djogi_search_seq FROM __djogi_tree"),
            "_djogi_search_seq must not leak into outer projection: {sql}"
        );
    }

    #[test]
    fn no_max_depth_no_predicate() {
        // Without `with_max_depth`, the recursive term's WHERE is
        // either absent (no user filter) or carries only the user
        // filter — never the depth probe.
        let qs = root();
        let acc = build_recursive_select(qs);
        let sql = acc.sql();
        assert!(
            !sql.contains("parent.depth <"),
            "no depth cap must not emit `parent.depth <`: {sql}"
        );
    }

    #[test]
    fn with_max_depth_emits_depth_predicate_and_binds_n() {
        let qs = root().with_max_depth(5);
        let acc = build_recursive_select(qs);
        let sql = acc.sql();
        assert!(
            sql.contains("parent.depth < $2"),
            "max_depth must bind as $2 (root_id is $1): {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            2,
            "max_depth + root_id = 2 binds, got {}",
            acc.bind_count()
        );
    }

    #[test]
    fn search_breadth_first_emits_clause_and_orders_outer() {
        // SEARCH BFS emits the `SEARCH BREADTH FIRST BY <col> SET
        // _djogi_search_seq` clause AND prepends `ORDER BY
        // _djogi_search_seq` on the outer SELECT so callers see BFS
        // order without an explicit `order_by`.
        let qs = root();
        let qs = qs.search_breadth_first_by(FieldRef::<MiniTree, String>::new("label"));
        let acc = build_recursive_select(qs);
        let sql = acc.sql();
        assert!(
            sql.contains("SEARCH BREADTH FIRST BY label SET _djogi_search_seq"),
            "BFS clause must be emitted on the CTE: {sql}"
        );
        assert!(
            sql.contains("ORDER BY _djogi_search_seq"),
            "outer SELECT must order by the search seq column: {sql}"
        );
    }

    #[test]
    fn search_depth_first_emits_dfs_keyword() {
        let qs = root().search_depth_first_by(FieldRef::<MiniTree, String>::new("label"));
        let acc = build_recursive_select(qs);
        let sql = acc.sql();
        assert!(
            sql.contains("SEARCH DEPTH FIRST BY label SET _djogi_search_seq"),
            "DFS clause must be emitted on the CTE: {sql}"
        );
    }

    #[test]
    fn search_modes_are_mutually_exclusive_last_wins() {
        let qs = root()
            .search_breadth_first_by(FieldRef::<MiniTree, String>::new("label"))
            .search_depth_first_by(FieldRef::<MiniTree, String>::new("label"));
        let acc = build_recursive_select(qs);
        let sql = acc.sql();
        // Last call (DFS) wins — BFS clause is gone, DFS clause is
        // present.
        assert!(
            !sql.contains("BREADTH FIRST"),
            "DFS after BFS must drop the BFS clause: {sql}"
        );
        assert!(
            sql.contains("DEPTH FIRST"),
            "last search-mode call must win: {sql}"
        );
    }

    #[test]
    fn count_terminal_wraps_in_count_subquery() {
        let qs = root();
        let acc = build_recursive_count(qs);
        let sql = acc.sql();
        assert!(
            sql.starts_with("SELECT COUNT(*) FROM ("),
            "count terminal wraps the recursive CTE: {sql}"
        );
        assert!(
            sql.ends_with(") AS sub"),
            "count subquery must close with `) AS sub`: {sql}"
        );
    }

    #[test]
    fn exists_terminal_wraps_with_limit_one() {
        let qs = root();
        let acc = build_recursive_exists(qs);
        let sql = acc.sql();
        assert!(
            sql.starts_with("SELECT EXISTS ("),
            "exists terminal wraps the recursive CTE: {sql}"
        );
        assert!(
            sql.contains("LIMIT 1)"),
            "exists subquery must include LIMIT 1: {sql}"
        );
    }

    #[test]
    fn anchor_binds_root_id_as_dollar_one() {
        let qs = root();
        let acc = build_recursive_select(qs);
        let sql = acc.sql();
        assert!(
            sql.contains("mini_trees.id = $1"),
            "anchor must bind root_id as $1: {sql}"
        );
        assert!(
            acc.bind_count() >= 1,
            "at least one bind expected (root_id), got {}",
            acc.bind_count()
        );
    }

    #[test]
    fn cte_column_list_includes_depth_followed_by_model_columns() {
        // The CTE column list must order `depth` first so the
        // anchor's `SELECT 0, <cols...>` and the recursive's
        // `SELECT parent.depth + 1, <child.cols...>` line up by
        // ordinal — Postgres validates the SET shape on every
        // recursive iteration.
        let qs = root();
        let acc = build_recursive_select(qs);
        let sql = acc.sql();
        assert!(
            sql.contains("__djogi_tree (depth, id, parent_id, label)"),
            "CTE column list must be (depth, <model cols...>): {sql}"
        );
    }
}
