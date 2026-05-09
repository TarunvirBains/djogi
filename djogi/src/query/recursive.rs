//! `RecursiveQuerySet<T>` — typed recursive-CTE query builder for
//! tree-shaped models. Phase 8-Zero Cluster B2 (T9 + T10 + T11 + T11b)
//! and Cluster B3 (T13 + T13a).
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
//! -- DESCENDANTS (one edge)
//! WITH RECURSIVE __djogi_tree (depth, path, <cols...>) AS (
//!     SELECT 0, ARRAY[]::text[], <cols...>
//!     FROM <table>
//!     WHERE id = $1
//!   UNION ALL
//!     SELECT parent.depth + 1, parent.path || ARRAY['<edge_col>'],
//!            <child.cols...>
//!     FROM <table> child
//!     JOIN __djogi_tree parent ON child.<edge_col> = parent.id
//!     [WHERE <user_filter>]
//!     [AND parent.depth < $n]
//! ) [SEARCH BREADTH FIRST BY <col> SET __djogi_search_seq]
//!   CYCLE id SET is_cycle USING cycle_path
//! SELECT <cols...> FROM __djogi_tree
//! WHERE NOT is_cycle
//! [ORDER BY <__djogi_search_seq,> <user_order>]
//! ```
//!
//! For `tree_ancestors` the join condition flips to
//! `parent.<edge_col> = child.id` — child walks up, parent has the FK
//! pointing at child.
//!
//! For `full_ancestors` (B3 — T13a) the recursive term consolidates
//! every self-FK edge into a single recursive SELECT and fans the
//! per-edge alternatives out through a non-recursive
//! `JOIN LATERAL (... UNION ALL ...) child ON TRUE` subquery. Each
//! lateral alternative tags its T-row with a synthetic
//! `__djogi_edge_label` text column, which the outer SELECT splices
//! into `path` so callers can distinguish `["mother_id",
//! "father_id"]` (maternal grandfather) from `["father_id",
//! "mother_id"]` (paternal grandmother). This shape satisfies
//! Postgres's "exactly one self-reference in the recursive term"
//! rule for multi-edge models.
//!
//! # SQL invariants
//!
//! - **`UNION ALL`**, never `UNION` — multiplicity preservation is
//!   load-bearing for `full_ancestors`. Wright-style kinship
//!   coefficients sum independent connecting paths through common
//!   ancestors, so the same ancestor reached by two distinct edge
//!   sequences must appear twice. `UNION` would dedup and silently
//!   drop those rows.
//! - **`CYCLE id SET is_cycle USING cycle_path`** is mandatory. Postgres
//!   manages both `is_cycle` and the cycle-detection `cycle_path`
//!   array automatically — they do not appear in our manual column
//!   list. The outer `WHERE NOT is_cycle` strips the cycle-detection
//!   sentinel rows from output. The cycle-detection column is named
//!   `cycle_path` (not `path`) so it does not collide with our user-
//!   visible `path: text[]` column that records the edge-name
//!   sequence from root to the current node.
//! - **`SEARCH ... BY <col> SET __djogi_search_seq`** emits only when
//!   the caller invoked
//!   [`search_breadth_first_by`](RecursiveQuerySet::search_breadth_first_by) /
//!   [`search_depth_first_by`](RecursiveQuerySet::search_depth_first_by).
//!   The internal sequence column `__djogi_search_seq` is macro-internal —
//!   the `__djogi_` prefix is framework-reserved (see
//!   `docs/spec/reserved-identifiers.md`), so it cannot collide with
//!   adopter model fields. It is never projected into the outer SELECT,
//!   but the outer `ORDER BY` references it so callers see BFS / DFS
//!   order without an explicit `order_by` call.
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
//!
//! # Visages do not implement tree queries (Phase 8-Zero T14b)
//!
//! [`VisageQuerySet`](crate::query::VisageQuerySet) intentionally does
//! NOT carry `tree_descendants` / `tree_ancestors` / `full_ancestors` in
//! v0.1.0. Recursive walks need the full row materialised at every step
//! (to follow the self-FK column from one row to the next), which is
//! exactly the column set a visage *narrows away*. A visage that drops
//! the self-FK column has no edge to walk; one that keeps the self-FK
//! but drops every other column is no different from a plain
//! `RecursiveQuerySet<T>` filtered through a SELECT projection — the
//! latter is what callers should reach for today.
//!
//! Future work could surface a `VisageRecursiveQuerySet` variant that
//! validates self-FK presence at the boundary and projects the visage's
//! column set onto the outer SELECT after the CTE materialises. Out of
//! scope for v0.1.0; the recursive-CTE surface stays exclusively on
//! `Model` and `QuerySet`.
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

/// Internal sequence column the `SEARCH BREADTH FIRST BY` /
/// `SEARCH DEPTH FIRST BY` clauses populate. Postgres assigns this
/// column on the recursive CTE; the outer `ORDER BY` then sorts on
/// it to surface BFS / DFS order to the caller. Macro-internal — the
/// `__djogi_` prefix is reserved by the framework (see
/// `docs/spec/reserved-identifiers.md`) so user columns cannot
/// collide. The constant is grep-able and shared between the
/// SEARCH-clause emit site and the outer ORDER BY emit site (a typo
/// at either would otherwise produce a `42703 column undefined`
/// Postgres error at runtime, not at compile time).
const SEARCH_SEQ_COL: &str = "__djogi_search_seq";

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
    /// `SEARCH BREADTH FIRST BY <col> SET __djogi_search_seq`.
    Breadth(&'static str),
    /// `SEARCH DEPTH FIRST BY <col> SET __djogi_search_seq`.
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
    /// Self-FK edges this walk traverses. Single-edge constructors
    /// ([`from_path`](Self::from_path) — used by
    /// [`QuerySet::tree_descendants`](crate::query::QuerySet::tree_descendants)
    /// and [`Model::tree_descendants`]) populate exactly one edge.
    /// Multi-edge constructors
    /// ([`from_paths`](Self::from_paths) — used by
    /// [`Model::full_ancestors`](crate::model::Model::full_ancestors))
    /// populate one entry per self-FK declared on `T`. The recursive
    /// term emits a single recursive SELECT and fans the per-edge
    /// alternatives out through a non-recursive `JOIN LATERAL (...
    /// UNION ALL ...) child ON TRUE` subquery — single-edge walks
    /// degenerate to one alternative (no inner `UNION ALL` inside the
    /// lateral) and behave exactly as the B2 single-edge form.
    ///
    /// An empty `edges` Vec is invalid — every constructor enforces
    /// `!edges.is_empty()` either at debug-assert time (the typed
    /// `from_path` / `from_paths` callers) or at terminal time
    /// (`Model::full_ancestors` on a model with `self_fk_count() == 0`).
    /// Each `RelationPath`'s identifier strings are validated at
    /// macro-emission via `__make_relation_path`, so pushing the
    /// `source_column` directly into emitted SQL is safe.
    pub(crate) edges: Vec<RelationPath<T, T>>,
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
    /// implicit `__djogi_search_seq` term so the user's ordering
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
        // `RelationPath` is `Debug`, but we render only the column
        // names — which is the load-bearing field for SQL emission
        // and matches the pre-B3 `edge_column: &'static str` debug
        // shape callers may rely on in test output.
        let edge_columns: Vec<&'static str> =
            self.edges.iter().map(|e| e.source_column()).collect();
        f.debug_struct("RecursiveQuerySet")
            .field("table", &T::table_name())
            .field("direction", &self.direction)
            .field("edge_columns", &edge_columns)
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
            edges: vec![path],
            root_id: Box::new(root_id),
            condition: Condition::True,
            ordering: Vec::new(),
            max_depth: None,
            search_mode: None,
            _model: PhantomData,
        }
    }

    /// Multi-edge constructor — the
    /// [`Model::full_ancestors`](crate::model::Model::full_ancestors)
    /// entry point. Phase 8-Zero Cluster B3 (T13a).
    ///
    /// One [`RelationPath<T, T>`] per self-FK edge declared on `T`;
    /// the SQL emitter then produces a single recursive SELECT that
    /// fans the per-edge alternatives out through a non-recursive
    /// `JOIN LATERAL (... UNION ALL ...) child ON TRUE` subquery —
    /// satisfying Postgres's "exactly one self-reference in the
    /// recursive term" rule while still walking every declared parent
    /// edge in a single CTE pass. `edges.len() == 1` degenerates to
    /// the same SQL the single-edge constructor produces.
    ///
    /// Accepts an empty `edges` Vec — terminals fail with a
    /// descriptive [`DjogiError::Query`] at execution time. Carrying
    /// the empty-edge state through the builder keeps the
    /// `Model::full_ancestors` return type uniform (always
    /// `RecursiveQuerySet<T>`, never `Result<RecursiveQuerySet<T>, _>`)
    /// so callers can compose `.with_max_depth(...)` /
    /// `.fetch_all(...)` chains uniformly across `self_fk_count()`
    /// values 0 / 1 / 2+.
    pub(crate) fn from_paths(
        paths: Vec<RelationPath<T, T>>,
        root_id: T::Pk,
        direction: RecursiveDirection,
    ) -> Self
    where
        T::Pk: ToSql + Sync + Send + 'static,
    {
        debug_assert!(
            paths
                .iter()
                .all(|p| matches!(p.kind(), RelationKind::ForeignKey | RelationKind::OneToOne)),
            "RecursiveQuerySet requires ForeignKey / OneToOne self-FKs only",
        );
        Self {
            direction,
            edges: paths,
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

    /// Emit `SEARCH BREADTH FIRST BY <col> SET __djogi_search_seq` on
    /// the CTE.
    ///
    /// Postgres annotates each recursive row with a sequence value the
    /// outer SELECT's `ORDER BY __djogi_search_seq` then sorts by; this
    /// terminal automatically prepends that ORDER BY (the user's
    /// `order_by` clauses, if any, append after as tiebreakers) so
    /// callers see BFS-traversal order without writing the order term
    /// by hand.
    ///
    /// Mutually exclusive with [`search_depth_first_by`](Self::search_depth_first_by) —
    /// last call wins. Type-state would be heavy for v0.1.0; mutual
    /// exclusion is documented behaviour.
    ///
    /// `__djogi_search_seq` is macro-internal — the `__djogi_` prefix
    /// is framework-reserved (see
    /// `docs/spec/reserved-identifiers.md`), so a model field cannot
    /// collide with the synthetic search column.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn search_breadth_first_by<V>(mut self, field: FieldRef<T, V>) -> Self {
        self.search_mode = Some(SearchMode::Breadth(field.column()));
        self
    }

    /// Emit `SEARCH DEPTH FIRST BY <col> SET __djogi_search_seq` on
    /// the CTE — DFS sibling of
    /// [`search_breadth_first_by`](Self::search_breadth_first_by).
    ///
    /// Same auto-prepended outer `ORDER BY __djogi_search_seq`,
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
) -> Result<SqlAccumulator, DjogiError> {
    build_recursive_inner::<T>(qs, RecursiveProjection::Rows)
}

/// `SELECT COUNT(*) FROM (...)` over the same CTE — wraps the row form
/// in a subquery so cycle stripping and the SEARCH ORDER BY (ignored
/// for COUNT but harmless) survive the count rewrite.
pub(crate) fn build_recursive_count<T: Model + FromPgRow>(
    qs: RecursiveQuerySet<T>,
) -> Result<SqlAccumulator, DjogiError> {
    build_recursive_inner::<T>(qs, RecursiveProjection::Count)
}

/// `SELECT EXISTS(SELECT 1 FROM (...) LIMIT 1)` — same wrap as count
/// but optimised for the early-exit semantics of EXISTS.
pub(crate) fn build_recursive_exists<T: Model + FromPgRow>(
    qs: RecursiveQuerySet<T>,
) -> Result<SqlAccumulator, DjogiError> {
    build_recursive_inner::<T>(qs, RecursiveProjection::Exists)
}

/// Same recursive-CTE shape as [`build_recursive_select`], with the
/// outer SELECT extended to project the CTE's `depth` and `path`
/// columns alongside `T`'s own column list. Phase 8-Zero Cluster B3
/// (T13).
pub(crate) fn build_recursive_select_with_paths<T: Model + FromPgRow>(
    qs: RecursiveQuerySet<T>,
) -> Result<SqlAccumulator, DjogiError> {
    build_recursive_inner::<T>(qs, RecursiveProjection::RowsWithDepthAndPath)
}

/// Outer projection mode for the shared recursive-CTE emitter.
///
/// Four terminals share the recursive-CTE shape (anchor + recursive
/// term + CYCLE + optional SEARCH); only the outer SELECT differs.
/// Routing through this enum keeps the CTE definition emitted exactly
/// once across `fetch_all` / `fetch_all_with_paths` / `count` /
/// `exists`, avoiding the drift-by-copy hazard a four-way function
/// split would introduce.
#[derive(Debug, Clone, Copy)]
enum RecursiveProjection {
    /// Outer `SELECT <cols...>` — the row terminal.
    Rows,
    /// Outer `SELECT <cols...>, depth, path` — the
    /// `fetch_all_with_paths` terminal (B3 T13). Adds two trailing
    /// columns the row decoder pulls out by name (`depth`, `path`)
    /// without touching `T`'s own `FromPgRow` impl.
    RowsWithDepthAndPath,
    /// Outer `SELECT COUNT(*)` — wraps the row form in a subquery.
    Count,
    /// Outer `SELECT EXISTS (... LIMIT 1)` — wraps the row form.
    Exists,
}

fn build_recursive_inner<T: Model + FromPgRow>(
    qs: RecursiveQuerySet<T>,
    projection: RecursiveProjection,
) -> Result<SqlAccumulator, DjogiError> {
    let mut acc = SqlAccumulator::new("");

    // The Count / Exists wraps need an outer expression around the row
    // SELECT. Open the wrap before the WITH so bind ordering stays
    // stable (the WITH's binds are still $1.. counted from the start).
    match projection {
        RecursiveProjection::Rows | RecursiveProjection::RowsWithDepthAndPath => {}
        RecursiveProjection::Count => acc.push_sql("SELECT COUNT(*) FROM ("),
        RecursiveProjection::Exists => acc.push_sql("SELECT EXISTS ("),
    }

    // ── WITH RECURSIVE __djogi_tree (depth, path, <cols...>) AS ( ────────
    //
    // The CTE column list orders `depth` and `path` first — the
    // anchor's `SELECT 0, ARRAY[]::text[], <cols...>` and the
    // recursive's `SELECT parent.depth + 1, parent.path ||
    // ARRAY['<edge>'], <child.cols...>` line up by ordinal, which
    // Postgres validates on every recursive iteration.
    acc.push_sql("WITH RECURSIVE __djogi_tree (depth, path, ");
    acc.push_sql(<T as FromPgRow>::COLUMN_LIST);
    acc.push_sql(") AS (");

    // ── Anchor: SELECT 0, ARRAY[]::text[], <cols...> FROM <table> WHERE id = $1
    //
    // The anchor's `path` is the empty `text[]` — every recursive
    // step appends one edge name, so the path length on a row equals
    // the number of edges traversed from the root, which is exactly
    // the row's `depth`.
    acc.push_sql("SELECT 0, ARRAY[]::text[], ");
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

    // ── UNION ALL — single recursive term, all edges fanned out ──────────
    //
    // `UNION ALL` (not `UNION`) is load-bearing for `full_ancestors`
    // multiplicity preservation. With multiple edges, the same
    // ancestor reached by two distinct edge sequences (e.g. a common
    // ancestor on both maternal and paternal sides of a pedigree)
    // must surface twice; `UNION` would dedup and silently break
    // Wright-style kinship coefficient computations.
    //
    // **Single recursive reference invariant.** Postgres rejects
    // recursive CTEs whose recursive term contains the CTE name more
    // than once ("recursive reference must not appear more than
    // once") AND those whose first-from-the-left UNION-ALL operand
    // already contains a recursive reference ("recursive reference
    // ... must not appear within its non-recursive term"). Multi-edge
    // walks therefore consolidate every edge into a single recursive
    // SELECT that joins `__djogi_tree parent` once and threads edge
    // selection through a non-recursive `LATERAL` subquery whose
    // `UNION ALL` enumerates per-edge candidates over `T`. Each
    // alternative inside the lateral picks T-rows reachable through
    // exactly one edge (`mother_id` or `father_id` etc.) and tags
    // them with a synthetic `__djogi_edge_label` text column the
    // outer SELECT splices into `path`. Multi-path arrivals are
    // preserved because each lateral alternative emits its own row
    // — UNION ALL never deduplicates.
    //
    // `qs` is consumed by value here, so each field can be moved out
    // directly — `condition` (a `Condition` enum tree) and `ordering`
    // (a `Vec<OrderExpr>`) used to be cloned out of historical caution,
    // but neither is reused after this point and `qs` itself is dropped
    // at function return. Moving avoids one heap allocation per
    // terminal call for non-trivial filter trees.
    let direction = qs.direction;
    let max_depth = qs.max_depth;
    let condition = qs.condition;
    let ordering = qs.ordering;
    let search_mode = qs.search_mode;
    let edges = qs.edges;

    let has_user_filter = !condition.is_vacuously_true();
    let has_depth_cap = max_depth.is_some();

    acc.push_sql(
        " UNION ALL SELECT parent.depth + 1, parent.path || ARRAY[child.__djogi_edge_label], ",
    );
    push_qualified_columns::<T>(&mut acc, "child");
    acc.push_sql(" FROM __djogi_tree parent JOIN LATERAL (");
    for (i, edge) in edges.iter().enumerate() {
        if i > 0 {
            acc.push_sql(" UNION ALL ");
        }
        // Per-edge alternative — pick T-rows reachable from `parent`
        // through exactly this edge column. `t` is the lateral
        // alias; the outer SELECT exposes it as `child` so user
        // filters / column projection remain stable across the
        // single-edge and multi-edge paths.
        acc.push_sql("SELECT ");
        push_qualified_columns::<T>(&mut acc, "t");
        acc.push_sql(", '");
        // `source_column` was identifier-validated at
        // `__make_relation_path` — ASCII alphanumeric + underscores
        // only, so embedding it inside a single-quoted SQL literal is
        // safe (no quote characters can appear inside the value).
        acc.push_sql(edge.source_column());
        acc.push_sql("'::text AS __djogi_edge_label FROM ");
        acc.push_sql(T::table_name());
        acc.push_sql(" t WHERE ");
        match direction {
            // Descendants: walk down. Parent's id matches child's
            // edge column.
            RecursiveDirection::Descendants => {
                acc.push_sql("t.");
                acc.push_sql(edge.source_column());
                acc.push_sql(" = parent.id");
            }
            // Ancestors: walk up. Parent's edge column points at
            // child's id.
            RecursiveDirection::Ancestors => {
                acc.push_sql("parent.");
                acc.push_sql(edge.source_column());
                acc.push_sql(" = t.id");
            }
        }
    }
    acc.push_sql(") child ON TRUE");

    // Recursive-term WHERE — user filter and / or depth cap.
    // Attaches once to the single consolidated SELECT (not per-edge
    // as the historical multi-branch shape did).
    if has_user_filter || has_depth_cap {
        acc.push_sql(" WHERE ");
        if has_user_filter {
            // The user filter references `T::Fields` columns,
            // which emit as bare names. The lateral exposes T's
            // columns via the `child` alias — qualifying the
            // user's predicate as `child.<col>` keeps Postgres
            // from raising `42702 column reference ambiguous`
            // against the `__djogi_tree parent` side of the JOIN,
            // which exposes the same column names through the
            // same alias scope.
            // Phase 8eta PR2b — `emit_condition` borrow-walks; the
            // `?` propagates `PortablePredicateError` to the surrounding
            // builder. `condition` here is a legacy `Condition` (not a
            // `Q<T>`) — reachable via the recursive-CTE path which
            // pre-dates the `Q<T>` substrate.
            emit_condition(&mut acc, &condition, Some("child")).map_err(crate::DjogiError::from)?;
        }
        if has_depth_cap {
            if has_user_filter {
                acc.push_sql(" AND ");
            }
            acc.push_sql("parent.depth < ");
            // `parent.depth` is INTEGER (int4) in Postgres — bind
            // as `i32` (not `i64`) so `tokio_postgres` accepts the
            // encoding. `WrongType { postgres: Int4, rust: "i64" }`
            // is a hard protocol-level failure, not a coercion
            // warning. `u32 → i32` is well-defined for u32 ≤
            // i32::MAX (saturating clamp for the unrealistic case
            // of u32 > i32::MAX, which would mean a recursion-depth
            // cap above ~2 billion — far past any sensible value).
            let n = max_depth.expect("has_depth_cap implies max_depth is Some");
            let n_i32 = i32::try_from(n).unwrap_or(i32::MAX);
            acc.push_bind(n_i32);
        }
    }

    // ── ) [SEARCH ...] CYCLE id SET is_cycle USING cycle_path ────────────
    acc.push_sql(")");
    if let Some(mode) = search_mode {
        acc.push_sql(mode.keyword());
        acc.push_sql(mode.column());
        acc.push_sql(" SET ");
        acc.push_sql(SEARCH_SEQ_COL);
    }
    // CYCLE: Postgres detects cycles using `cycle_path`, marks them
    // in `is_cycle`, and stops recursion at the marked row. Without
    // this clause, a malformed self-FK chain (cycle introduced by
    // buggy application code or a manual SQL edit) would loop
    // forever. The `id` column name + the synthetic `cycle_path` /
    // `is_cycle` columns are managed entirely by Postgres — they do
    // not appear in our column list and cannot collide with user
    // fields. We name the cycle-detection array `cycle_path` (not
    // `path`) to avoid collision with our user-visible `path: text[]`
    // column that records the edge-name sequence from root to the
    // current node.
    acc.push_sql(" CYCLE id SET is_cycle USING cycle_path");

    // ── Outer SELECT ──────────────────────────────────────────────────────
    match projection {
        RecursiveProjection::Rows => {
            acc.push_sql(" SELECT ");
            acc.push_sql(<T as FromPgRow>::COLUMN_LIST);
            acc.push_sql(" FROM __djogi_tree WHERE NOT is_cycle");
            push_outer_order_by(&mut acc, search_mode.is_some(), &ordering);
        }
        RecursiveProjection::RowsWithDepthAndPath => {
            // Outer SELECT projects T's columns first (so the row's
            // ordinals 0..N still match `T::COLUMNS`), then `depth`
            // and `path` as trailing columns the terminal reads by
            // name. Reading by name (not ordinal) avoids the
            // off-by-one hazard if `T::COLUMNS` ever grows.
            acc.push_sql(" SELECT ");
            acc.push_sql(<T as FromPgRow>::COLUMN_LIST);
            acc.push_sql(", depth, path FROM __djogi_tree WHERE NOT is_cycle");
            push_outer_order_by(&mut acc, search_mode.is_some(), &ordering);
        }
        RecursiveProjection::Count => {
            // Inner SELECT inside the COUNT subquery — projects only
            // `1` since we just need rows to count. ORDER BY would
            // be discarded by the COUNT wrap; SEARCH-ordering
            // likewise has no observable effect on COUNT, so we
            // omit both for a minimal-shape inner query.
            acc.push_sql(" SELECT 1 FROM __djogi_tree WHERE NOT is_cycle) AS sub");
        }
        RecursiveProjection::Exists => {
            // `EXISTS(... LIMIT 1)` — Postgres stops scanning at the
            // first match. Same minimal projection as the count path.
            acc.push_sql(" SELECT 1 FROM __djogi_tree WHERE NOT is_cycle LIMIT 1)");
        }
    }

    Ok(acc)
}

/// Emit the outer `ORDER BY` clause shared by [`RecursiveProjection::Rows`]
/// and [`RecursiveProjection::RowsWithDepthAndPath`].
///
/// SEARCH-ordering first (so BFS / DFS works without an explicit
/// `order_by`), then the user's ordering as tiebreakers. Either alone
/// is valid; both together is valid SQL and matches Django's
/// append-tiebreakers convention. Extracted into a helper so the two
/// row projections share the exact same ordering shape — adding a new
/// row terminal in B4 will not have to re-derive the rule.
fn push_outer_order_by(acc: &mut SqlAccumulator, has_search_order: bool, ordering: &[OrderExpr]) {
    let has_user_order = !ordering.is_empty();
    if !has_search_order && !has_user_order {
        return;
    }
    acc.push_sql(" ORDER BY ");
    if has_search_order {
        acc.push_sql(SEARCH_SEQ_COL);
    }
    if has_user_order {
        if has_search_order {
            acc.push_sql(", ");
        }
        for (i, o) in ordering.iter().enumerate() {
            if i > 0 {
                acc.push_sql(", ");
            }
            // Outer SELECT runs against `__djogi_tree`, which
            // exposes T's columns under their bare names — same
            // shape as the plain `QuerySet` ordering emit, so
            // `parent_table = None`.
            o.emit(acc, None);
        }
    }
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
            check_edges_present::<T>(&self.edges)?;
            auto_set_tenant::<T>(ctx).await?;
            let acc = build_recursive_select(self)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let rows = ctx.query_all(&sql, &params).await?;
            rows.iter()
                .map(|r| T::from_pg_row(r))
                .collect::<Result<Vec<T>, _>>()
        }
    }

    /// Materialise every reachable row paired with its **depth** and
    /// **path** — the edge-name sequence from the root to that row.
    /// Phase 8-Zero Cluster B3 (T13).
    ///
    /// `depth` is the `i32` count of recursive hops from the anchor;
    /// `path` is the `Vec<String>` of edge column names appended one
    /// per recursive step. For a single-edge `tree_descendants` walk
    /// every row's `path` is `["<edge_col>"; depth]` — uniform but
    /// useful when the caller needs the edge name without re-deriving
    /// it from the queryset shape.
    ///
    /// For a multi-edge
    /// [`Model::full_ancestors`](crate::model::Model::full_ancestors)
    /// walk every step independently picks which edge it followed, so
    /// `path` becomes the load-bearing distinguisher: `["mother_id",
    /// "father_id"]` is "follow the mother edge from the root, then
    /// the father edge from there" — the maternal grandfather. The
    /// reverse — `["father_id", "mother_id"]` — is the paternal
    /// grandmother. Wright kinship calculations sum coefficients
    /// across `path` permutations, so multiplicity is preserved
    /// (every distinct path materialises a separate row even when it
    /// reaches the same ancestor twice).
    ///
    /// The tuple shape `(T, i32, Vec<String>)` is intentional —
    /// pre-1.0 we keep the surface narrow; a typed wrapper struct
    /// would lock in field names that benchmark callers may want
    /// renamed once shell ergonomics surface real usage. The
    /// projection is `<cols...>, depth, path` so the row decoder
    /// reads `depth` / `path` by name (not ordinal), insulating
    /// callers from `T::COLUMNS` length drift.
    pub fn fetch_all_with_paths<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Vec<(T, i32, Vec<String>)>, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
    {
        async move {
            check_edges_present::<T>(&self.edges)?;
            auto_set_tenant::<T>(ctx).await?;
            let acc = build_recursive_select_with_paths(self)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let rows = ctx.query_all(&sql, &params).await?;
            rows.iter()
                .map(|r| {
                    // `T::from_pg_row` reads ordinals 0..N over
                    // `T::COLUMNS`. The CTE's `depth` and `path`
                    // are the trailing two columns — read them by
                    // name to stay decoupled from `T::COLUMNS`'
                    // exact length.
                    let model = T::from_pg_row(r)?;
                    let depth: i32 = r.try_get("depth").map_err(DjogiError::from)?;
                    let path: Vec<String> = r.try_get("path").map_err(DjogiError::from)?;
                    Ok((model, depth, path))
                })
                .collect::<Result<Vec<_>, DjogiError>>()
        }
    }

    /// Return the first reachable row, or `None` if the walk yields
    /// no rows.
    ///
    /// Emits the same recursive CTE as `fetch_all` with a tailored
    /// outer `LIMIT 1`. The `LIMIT` lives on the outer SELECT — after
    /// CYCLE filtering and SEARCH / user ORDER BY — so callers get
    /// the *first* row by their declared traversal order, not an
    /// arbitrary planner choice. Without `LIMIT 1` the underlying
    /// `query_opt` errors when the walk yields more than one row.
    pub fn first<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Option<T>, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
    {
        async move {
            check_edges_present::<T>(&self.edges)?;
            auto_set_tenant::<T>(ctx).await?;
            let mut acc = build_recursive_select(self)?;
            acc.push_sql(" LIMIT 1");
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
            check_edges_present::<T>(&self.edges)?;
            auto_set_tenant::<T>(ctx).await?;
            let acc = build_recursive_count(self)?;
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
            check_edges_present::<T>(&self.edges)?;
            auto_set_tenant::<T>(ctx).await?;
            let acc = build_recursive_exists(self)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let row = ctx.query_one(&sql, &params).await?;
            try_get_scalar::<bool>(&row, 0)
        }
    }
}

// ── Empty-edges guard ──────────────────────────────────────────────────────

/// Guard every terminal against an empty `edges` Vec — the
/// [`Model::full_ancestors`](crate::model::Model::full_ancestors)
/// constructor on a model with `self_fk_count() == 0` carries
/// `edges: vec![]` through the builder so the caller's
/// `.with_max_depth(...)` / `.fetch_all(...)` chain still type-checks.
/// At terminal time we surface the misuse as a descriptive
/// [`DjogiError::Query`] before any SQL is built. Phase 8-Zero
/// Cluster B3 (T13a).
///
/// The error message names the model and points at the requirement —
/// callers either declare a self-FK or fall back to
/// [`Model::tree_descendants`] / [`Model::tree_ancestors`] which
/// already communicate the requirement at construction time.
fn check_edges_present<T: Model>(edges: &[RelationPath<T, T>]) -> Result<(), DjogiError> {
    if edges.is_empty() {
        return Err(DjogiError::Validation(format!(
            "model '{}' has no self-FK; full_ancestors requires at least one",
            T::table_name(),
        )));
    }
    Ok(())
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
        let acc = build_recursive_select(qs).expect("recursive select");
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
        // Descendants direction: the per-edge alternative inside the
        // lateral matches `t.<edge_col> = parent.id` (single self-FK
        // here is `parent_id`).
        assert!(
            sql.contains("t.parent_id = parent.id"),
            "descendants lateral alternative must filter t.parent_id = parent.id: {sql}"
        );
    }

    #[test]
    fn ancestors_flips_join_direction() {
        let qs = ancestors_root();
        let acc = build_recursive_select(qs).expect("recursive select");
        let sql = acc.sql();
        // Ancestors direction: the per-edge alternative inside the
        // lateral matches `parent.<edge_col> = t.id`.
        assert!(
            sql.contains("parent.parent_id = t.id"),
            "ancestors lateral alternative must filter parent.parent_id = t.id: {sql}"
        );
    }

    #[test]
    fn cycle_clause_is_always_emitted() {
        // CYCLE detection is mandatory — a malformed self-FK chain
        // must not loop forever even when the caller forgets
        // `with_max_depth`. B3 renames the cycle-detection array
        // column from `path` to `cycle_path` so it cannot collide
        // with our user-visible `path: text[]` column that records
        // edge-name sequences.
        let qs = root();
        let acc = build_recursive_select(qs).expect("recursive select");
        let sql = acc.sql();
        assert!(
            sql.contains("CYCLE id SET is_cycle USING cycle_path"),
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
        // simply `<T as FromPgRow>::COLUMN_LIST`. The plain `Rows`
        // projection does NOT include `depth` / `path`; those are
        // exclusive to the `RowsWithDepthAndPath` projection that
        // backs `fetch_all_with_paths`.
        let qs = root();
        let acc = build_recursive_select(qs).expect("recursive select");
        let sql = acc.sql();
        assert!(
            sql.contains(" SELECT id, parent_id, label FROM __djogi_tree"),
            "outer SELECT must project canonical columns: {sql}"
        );
        assert!(
            !sql.contains(", depth, path FROM __djogi_tree"),
            "depth/path columns must not leak into the plain Rows projection: {sql}"
        );
        assert!(
            !sql.contains("__djogi_search_seq FROM __djogi_tree"),
            "__djogi_search_seq must not leak into outer projection: {sql}"
        );
    }

    #[test]
    fn no_max_depth_no_predicate() {
        // Without `with_max_depth`, the recursive term's WHERE is
        // either absent (no user filter) or carries only the user
        // filter — never the depth probe.
        let qs = root();
        let acc = build_recursive_select(qs).expect("recursive select");
        let sql = acc.sql();
        assert!(
            !sql.contains("parent.depth <"),
            "no depth cap must not emit `parent.depth <`: {sql}"
        );
    }

    #[test]
    fn with_max_depth_emits_depth_predicate_and_binds_n() {
        let qs = root().with_max_depth(5);
        let acc = build_recursive_select(qs).expect("recursive select");
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
        // __djogi_search_seq` clause AND prepends `ORDER BY
        // __djogi_search_seq` on the outer SELECT so callers see BFS
        // order without an explicit `order_by`.
        let qs = root();
        let qs = qs.search_breadth_first_by(FieldRef::<MiniTree, String>::new("label"));
        let acc = build_recursive_select(qs).expect("recursive select");
        let sql = acc.sql();
        assert!(
            sql.contains("SEARCH BREADTH FIRST BY label SET __djogi_search_seq"),
            "BFS clause must be emitted on the CTE: {sql}"
        );
        assert!(
            sql.contains("ORDER BY __djogi_search_seq"),
            "outer SELECT must order by the search seq column: {sql}"
        );
    }

    #[test]
    fn search_depth_first_emits_dfs_keyword() {
        let qs = root().search_depth_first_by(FieldRef::<MiniTree, String>::new("label"));
        let acc = build_recursive_select(qs).expect("recursive select");
        let sql = acc.sql();
        assert!(
            sql.contains("SEARCH DEPTH FIRST BY label SET __djogi_search_seq"),
            "DFS clause must be emitted on the CTE: {sql}"
        );
    }

    #[test]
    fn search_modes_are_mutually_exclusive_last_wins() {
        let qs = root()
            .search_breadth_first_by(FieldRef::<MiniTree, String>::new("label"))
            .search_depth_first_by(FieldRef::<MiniTree, String>::new("label"));
        let acc = build_recursive_select(qs).expect("recursive select");
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
        let acc = build_recursive_count(qs).expect("recursive count");
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
        let acc = build_recursive_exists(qs).expect("recursive exists");
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
    fn first_terminal_appends_outer_limit_one() {
        let qs = root();
        let mut acc = build_recursive_select(qs).expect("recursive select");
        acc.push_sql(" LIMIT 1");
        let sql = acc.sql();
        assert!(
            sql.trim_end().ends_with(" LIMIT 1"),
            "first terminal must append outer LIMIT 1 so query_opt does not \
             error on multi-row recursive walks: {sql}"
        );
        assert!(
            !sql.contains("EXISTS"),
            "first must use the row projection (not the EXISTS wrap): {sql}"
        );
    }

    #[test]
    fn anchor_binds_root_id_as_dollar_one() {
        let qs = root();
        let acc = build_recursive_select(qs).expect("recursive select");
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
    fn cte_column_list_includes_depth_then_path_then_model_columns() {
        // The CTE column list must order `depth` and `path` first
        // so the anchor's `SELECT 0, ARRAY[]::text[], <cols...>` and
        // the recursive's
        // `SELECT parent.depth + 1, parent.path || ARRAY['<edge>'],
        // <child.cols...>` line up by ordinal — Postgres validates
        // the SET shape on every recursive iteration. B3 adds the
        // user-visible `path: text[]` column.
        let qs = root();
        let acc = build_recursive_select(qs).expect("recursive select");
        let sql = acc.sql();
        assert!(
            sql.contains("__djogi_tree (depth, path, id, parent_id, label)"),
            "CTE column list must be (depth, path, <model cols...>): {sql}"
        );
    }

    #[test]
    fn anchor_initialises_path_as_empty_text_array() {
        // The anchor row has `depth = 0` and `path = ARRAY[]::text[]`
        // — the empty path the recursive term then accumulates one
        // edge name into per step.
        let qs = root();
        let acc = build_recursive_select(qs).expect("recursive select");
        let sql = acc.sql();
        assert!(
            sql.contains("SELECT 0, ARRAY[]::text[], "),
            "anchor must initialise depth=0 and path=ARRAY[]::text[]: {sql}"
        );
    }

    #[test]
    fn recursive_term_appends_edge_name_to_path() {
        // The recursive term emits
        // `parent.path || ARRAY[child.__djogi_edge_label]` where the
        // synthetic `__djogi_edge_label` carries the edge-column
        // name as a text literal from inside the lateral. Each step
        // records which edge was followed, so callers can filter on
        // edge sequences (e.g. `path == ["mother_id", "father_id"]`).
        let qs = root();
        let acc = build_recursive_select(qs).expect("recursive select");
        let sql = acc.sql();
        assert!(
            sql.contains("parent.path || ARRAY[child.__djogi_edge_label]"),
            "recursive term must append the lateral's edge label: {sql}"
        );
        assert!(
            sql.contains("'parent_id'::text AS __djogi_edge_label"),
            "lateral must tag rows with the edge name as __djogi_edge_label: {sql}"
        );
        assert!(
            !sql.contains("child.id::text"),
            "B2's placeholder `child.id::text` must be gone: {sql}"
        );
    }

    #[test]
    fn fetch_all_with_paths_projects_depth_and_path_columns() {
        // The B3 `fetch_all_with_paths` terminal extends the outer
        // SELECT to project `<cols...>, depth, path` so the row
        // decoder reads the trailing two columns by name.
        let qs = root();
        let acc = build_recursive_select_with_paths(qs).expect("recursive select with paths");
        let sql = acc.sql();
        assert!(
            sql.contains("SELECT id, parent_id, label, depth, path FROM __djogi_tree"),
            "with-paths projection must trail depth and path columns: {sql}"
        );
    }

    // ── full_ancestors / multi-edge tests ────────────────────────────────

    /// Construct a multi-edge ancestor walker over two synthetic
    /// self-FKs `mother_id` + `father_id`. Models like this aren't
    /// expressible through the descriptor-derived `Model::full_ancestors`
    /// path in the test crate (no `#[model]` macro available here), so
    /// we exercise `from_paths` directly with hand-built relation paths.
    fn full_ancestors_two_edges() -> RecursiveQuerySet<MiniTree> {
        let edges = vec![
            crate::relation::__macro_support::__make_relation_path::<MiniTree, MiniTree>(
                "mother_id",
                "mini_trees",
                RelationKind::ForeignKey,
            ),
            crate::relation::__macro_support::__make_relation_path::<MiniTree, MiniTree>(
                "father_id",
                "mini_trees",
                RelationKind::ForeignKey,
            ),
        ];
        RecursiveQuerySet::<MiniTree>::from_paths(
            edges,
            HeerId::from_i64(1).unwrap(),
            RecursiveDirection::Ancestors,
        )
    }

    #[test]
    fn full_ancestors_two_edges_consolidates_into_single_recursive_term() {
        // Postgres restricts recursive CTEs to ONE self-reference in
        // the recursive term. Multi-edge walks therefore consolidate
        // every edge into a single recursive SELECT and enumerate
        // edge alternatives via a non-recursive `LATERAL (... UNION
        // ALL ...)` subquery. (B5 round-2 fixup — the original
        // per-edge UNION ALL form failed live with "recursive
        // reference must not appear more than once".)
        //
        // Total `UNION ALL` count is `1 + (N - 1)` for N edges:
        // - 1 between anchor and the consolidated recursive term
        // - N - 1 inside the LATERAL between the per-edge SELECTs
        let qs = full_ancestors_two_edges();
        let acc = build_recursive_select(qs).expect("recursive select");
        let sql = acc.sql();
        let union_count = sql.matches("UNION ALL").count();
        assert_eq!(
            union_count, 2,
            "two-edge ancestors: 1 UNION ALL between anchor + recursive term, \
             plus 1 inside the LATERAL between the two per-edge SELECTs: {sql}"
        );
        // Both edges live inside the LATERAL as separate SELECTs.
        assert!(
            sql.contains("parent.mother_id = t.id"),
            "first edge alternative must filter parent.mother_id = t.id: {sql}"
        );
        assert!(
            sql.contains("parent.father_id = t.id"),
            "second edge alternative must filter parent.father_id = t.id: {sql}"
        );
        assert!(
            sql.contains("'mother_id'::text AS __djogi_edge_label"),
            "first edge alternative must tag with mother_id label: {sql}"
        );
        assert!(
            sql.contains("'father_id'::text AS __djogi_edge_label"),
            "second edge alternative must tag with father_id label: {sql}"
        );
    }

    #[test]
    fn single_edge_emits_one_union_all_branch() {
        // Single-edge degenerate case — exactly one `UNION ALL`
        // (between anchor and the lone recursive branch). Confirms
        // the multi-edge refactor preserves single-edge SQL shape.
        let qs = root();
        let acc = build_recursive_select(qs).expect("recursive select");
        let sql = acc.sql();
        let union_count = sql.matches("UNION ALL").count();
        assert_eq!(
            union_count, 1,
            "single-edge walk must emit exactly 1 UNION ALL keyword: {sql}"
        );
    }

    #[test]
    fn empty_edges_errors_at_terminal_time() {
        // `Model::full_ancestors` on a model with `self_fk_count() == 0`
        // returns a `RecursiveQuerySet` with empty edges. Builder
        // chains type-check, but the terminal returns
        // `DjogiError::Validation` before any SQL is emitted.
        // Here we exercise the guard directly on `check_edges_present`
        // since reaching a real terminal needs a live DB context.
        let edges: Vec<RelationPath<MiniTree, MiniTree>> = Vec::new();
        let result = check_edges_present::<MiniTree>(&edges);
        let err = result.expect_err("empty edges must error");
        let msg = err.to_string();
        assert!(
            msg.contains("mini_trees"),
            "error must name the model's table: {msg}"
        );
        assert!(
            msg.contains("full_ancestors") && msg.contains("self-FK"),
            "error must point at full_ancestors and the self-FK requirement: {msg}"
        );
    }

    #[test]
    fn full_ancestors_two_edges_with_max_depth_binds_once() {
        // `with_max_depth` attaches once to the consolidated
        // recursive term (not per-edge as the historical multi-branch
        // shape did). Total binds = 1 root id + 1 depth cap = 2,
        // regardless of edge count. (B5 round-2 fixup.)
        let qs = full_ancestors_two_edges().with_max_depth(3);
        let acc = build_recursive_select(qs).expect("recursive select");
        let sql = acc.sql();
        assert!(
            sql.contains("parent.depth < $2"),
            "single recursive term => single depth bind at $2: {sql}"
        );
        assert!(
            !sql.contains("parent.depth < $3"),
            "second depth bind must not appear — only one recursive term: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            2,
            "expected 2 binds (root + 1 depth cap), got {}",
            acc.bind_count()
        );
    }
}
