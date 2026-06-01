//! `Model::materialize_closure` — populate a transitive-closure table for
//! a self-referential model. (T13b).
//! # What
//! A *closure table* stores `(source, ancestor, depth, path_count)` triples
//! pre-computed from a self-referential edge graph. It is the production-
//! scale answer for tree queries: an indexed lookup beats a recursive CTE
//! by orders of magnitude once the source table has more than a handful
//! of rows. Every adopter doing tree queries at non-trivial scale needs
//! it; without a framework helper, every adopter hand-rolls the walker.
//! [`Model::materialize_closure`](crate::model::Model::materialize_closure)
//! ships the helper. Adopters declare a closure model with the same five
//! columns the helper writes (`source`, `ancestor`, `depth`, `path_count`,
//! plus a unique index on `(source, ancestor, depth)`), implement the
//! [`ClosureModel`] trait to surface the column names, and call
//! `T::materialize_closure::<MyClosure>(ctx, opts).await?` to (re)populate
//! the table.
//! # SQL shape
//! ```sql
//! WITH inserted AS (
//!     INSERT INTO <closure_table> (
//!         <source_col>, <ancestor_col>, <depth_col>, <path_count_col>
//!     )
//!     WITH RECURSIVE __djogi_closure AS (
//!         -- anchor: every source row is its own ancestor at depth 0
//!         SELECT s.id AS source_id, s.id AS ancestor_id, 0 AS depth,
//!                ARRAY[]::text[] AS path
//!         FROM <source_table> s
//!         WHERE <root_predicate or TRUE>
//!         UNION ALL
//!         -- recursive: walk every self-FK edge inside ONE recursive
//!         -- term. The closure CTE only carries
//!         -- `(source_id, ancestor_id, depth, path)` — it does not
//!         -- project T's self-FK columns — so we double-join the
//!         -- source table: first to resolve the current ancestor row
//!         -- (`a`) by `closure.ancestor_id`, then to follow each edge
//!         -- column up to its parent row (`p`). A `CROSS JOIN LATERAL
//!         -- VALUES (...)` enumerates every self-FK edge of `T` in
//!         -- one fan-out so multi-edge models still satisfy
//!         -- Postgres's "exactly one self-reference" rule for
//!         -- recursive terms.
//!         SELECT closure.source_id, p.id, closure.depth + 1,
//!                closure.path || ARRAY[step.label]
//!         FROM __djogi_closure closure
//!         JOIN <source_table> a ON a.id = closure.ancestor_id
//!         CROSS JOIN LATERAL (VALUES
//!             (a.<edge_col_1>, '<edge_col_1>'::text),
//!             (a.<edge_col_2>, '<edge_col_2>'::text),
//!             ...
//!         ) AS step(pid, label)
//!         JOIN <source_table> p ON p.id = step.pid
//!         WHERE closure.depth < <max_depth>?
//!     ) CYCLE source_id, ancestor_id SET is_cycle USING cycle_path
//!     SELECT
//!         source_id, ancestor_id, depth,
//!         COUNT(*) AS path_count
//!     FROM __djogi_closure
//!     WHERE NOT is_cycle
//!     GROUP BY source_id, ancestor_id, depth
//!     ON CONFLICT (<source_col>, <ancestor_col>, <depth_col>)
//!     DO UPDATE SET <path_count_col> = EXCLUDED.<path_count_col>
//!     RETURNING <source_col>
//! )
//! SELECT COUNT(*) AS rows_written,
//!        COUNT(DISTINCT <source_col>) AS sources_visited
//! FROM inserted;
//! ```
//! ## Design notes baked into the SQL
//! - **Direction is ANCESTORS.** The closure table walks *up* from each
//!   source row to its transitive ancestors. The named driver for this
//!   helper is kinship / pedigree analysis, where the "every ancestor of
//!   every source row" frame is the natural shape.
//! - **Single recursive reference + `CROSS JOIN LATERAL VALUES`.**
//!   Postgres rejects recursive CTEs whose recursive term references
//!   the CTE name more than once. Multi-edge models (e.g.
//!   `mother_id` + `father_id` on an animal model) therefore enumerate
//!   self-FK edges via a `CROSS JOIN LATERAL (VALUES …) AS step(pid,
//! label)` clause that fans every edge column out into its own
//!   row pair — every distinct edge-sequence path still surfaces as
//!   its own CTE row, then the outer `GROUP BY` collapses
//!   `(source, ancestor, depth)` triples while surfacing the count as
//!   `path_count`. This preserves Wright-style multiplicity: an
//!   ancestor reachable by two distinct edge sequences shows up with
//!   `path_count = 2`. NULL-valued edge columns get filtered by the
//!   inner `JOIN T p ON p.id = step.pid` (NULL = anything is unknown).
//! - **Cycle column is `cycle_path`** (not `path`) so it does not
//!   collide with our user-visible edge-name accumulator.
//! - **`ON CONFLICT … DO UPDATE` replaces, it does not add.** Each
//!   helper invocation walks the current graph from scratch via the
//!   recursive CTE, so EXCLUDED's `path_count` is already the correct
//!   total of distinct paths between every `(source, ancestor, depth)`
//!   triple in the present graph state. The pre-existing closure row's
//!   value is the previous (possibly stale) total; replacing it with
//!   EXCLUDED keeps the closure aligned with whatever lives in the
//!   source table now. Re-running the helper twice in a row is therefore
//!   idempotent — the second run computes the same totals and writes
//!   them on top of themselves. An `additive` merge would double on
//!   straight rerun and over-count on every incremental rerun (because
//!   the recursive walk re-derives existing paths on top of new ones),
//!   so it is wrong on every callsite that matters.
//! - **`RETURNING <source_col>`** plus the outer `COUNT` /
//!   `COUNT(DISTINCT)` lets the helper report both `rows_written`
//!   (unique `(source, ancestor, depth)` triples touched) and
//!   `sources_visited` (distinct source rows whose ancestors were
//!   walked) in a single round trip — no second query against the
//!   closure table.
//! ## Required closure-table schema
//! Adopters must create the closure table with at least:
//! ```sql
//! CREATE TABLE <closure_table> (
//!     id           BIGINT PRIMARY KEY DEFAULT heerid_next(),
//!     <source>     <PK type> NOT NULL REFERENCES <source_table>(id) ON DELETE CASCADE,
//!     <ancestor>   <PK type> NOT NULL REFERENCES <source_table>(id) ON DELETE CASCADE,
//!     <depth>      INTEGER NOT NULL,
//!     <path_count> BIGINT NOT NULL,
//!     created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
//!     updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
//!     UNIQUE (<source>, <ancestor>, <depth>)
//! );
//! ```
//! `<path_count>` does not need a column-level `DEFAULT` — every row
//! the helper writes carries an explicit `COUNT(*)` value. Adopters
//! who add `DEFAULT 1` get the same runtime behavior but introduce a
//! drift point if they're maintaining a parallel descriptor (the
//! framework's migration projection does not synthesize column
//! defaults for user-declared fields, so the descriptor would carry
//! `default = None` against a live `DEFAULT 1` schema).
//! The `UNIQUE (<source>, <ancestor>, <depth>)` constraint is **load-
//! bearing** — `ON CONFLICT (...)` requires an exact match against a
//! unique constraint. The helper validates the column-name identifiers
//! at runtime via [`crate::ident::check_user_supplied_ident`]; the
//! unique constraint itself the framework cannot verify without
//! reaching the catalog, so the contract is stated in this doc and
//! surfaces as a Postgres `42P10` error if the constraint is missing.
#![allow(clippy::manual_async_fn)]

use crate::DjogiError;
use crate::context::DjogiContext;
use crate::ident::check_user_supplied_ident;
use crate::model::Model;
use crate::pg::accumulator::{SqlAccumulator, as_params};
use crate::pg::decode::try_get_scalar;
use crate::query::terminal::auto_set_tenant;
use postgres_types::ToSql;
use std::future::Future;

/// Builder-bag of options that govern one
/// [`Model::materialize_closure`](crate::model::Model::materialize_closure)
/// call.
/// Constructed via [`Default::default`] and tuned with the four setter
/// methods. A `Default::default`-bag walks every source row to its
/// natural depth — the right baseline for the initial population of a
/// closure table.
/// `Pk` is the source-model's primary-key Rust type; the parametric
/// shape avoids forcing every caller through a `Box<dyn ToSql>`
/// anti-pattern just to bound the `roots` Vec across `T::Pk` values
/// (`HeerId`, `RanjId`, `i32`, …).
#[derive(Debug)]
pub struct MaterializeClosureOptions<Pk: ToSql + Sync + Send + 'static> {
    /// Bound the recursive walk at this depth (`0`-based — depth `0` is
    /// the source row itself, depth `1` is its direct parent, …). When
    /// `None`, the walk runs to natural exhaustion or until the
    /// `CYCLE source_id, ancestor_id` clause's cycle detection halts
    /// it. Both termination paths are correct; reach for `max_depth`
    /// only when the closure table genuinely should not record
    /// ancestors past a fixed horizon (e.g. UI breadcrumb that never
    /// renders more than five generations).
    pub max_depth: Option<u32>,
    /// Walk closure only for these source rows. When `None`, the walk
    /// covers every row in the source table (anchor `WHERE TRUE`).
    /// Passing an empty `Some(vec![])` is equivalent to "no work to
    /// do" — the anchor `WHERE` evaluates to `FALSE` and the helper
    /// returns a zeroed [`MaterializeClosureReport`].
    /// Pre-1.0 we ship the simplest shape that closes the contract:
    /// a `Vec<T::Pk>` of source ids. A predicate-tree shape (composing
    /// arbitrary `Condition` expressions) is a post-publish addition
    /// Wright kinship use cases want "rebuild closure for these
    /// specific newly-added animals" which the Vec form covers
    /// directly.
    pub roots: Option<Vec<Pk>>,
}

/// `Default` impl that does *not* require `Pk: Default`.
/// `#[derive(Default)]` on a generic struct requires every type
/// parameter to itself implement `Default`. The actual fields
/// (`Option<u32>` and `Option<Vec<Pk>>`) both default to `None`
/// without ever needing a `Pk` value, so a hand-written impl side-
/// steps the unnecessary bound and lets adopters use
/// `MaterializeClosureOptions::<HeerId>::default` directly.
impl<Pk: ToSql + Sync + Send + 'static> Default for MaterializeClosureOptions<Pk> {
    fn default() -> Self {
        Self {
            max_depth: None,
            roots: None,
        }
    }
}

impl<Pk: ToSql + Sync + Send + 'static> MaterializeClosureOptions<Pk> {
    /// Set the recursive-walk depth cap. See [`Self::max_depth`].
    pub fn with_max_depth(mut self, n: u32) -> Self {
        self.max_depth = Some(n);
        self
    }

    /// Restrict the walk to this set of source rows. See [`Self::roots`].
    pub fn with_roots(mut self, ids: Vec<Pk>) -> Self {
        self.roots = Some(ids);
        self
    }
}

/// Summary of one [`Model::materialize_closure`](crate::model::Model::materialize_closure)
/// call.
/// Computed in the helper's single round trip via
/// `RETURNING <source_col>` plus an outer `COUNT` / `COUNT(DISTINCT)`
/// CTE wrap, so reporting these counters costs nothing extra — no
/// second query against the closure table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaterializeClosureReport {
    /// Number of `(source, ancestor, depth)` triples the `INSERT` /
    /// `ON CONFLICT DO UPDATE` touched. New triples count once;
    /// existing triples whose `path_count` was replaced from EXCLUDED
    /// also count once (Postgres returns the same set of rows from
    /// `RETURNING` for both branches).
    pub rows_written: u64,
    /// Number of distinct source rows whose ancestor chain was walked
    /// equivalent to the size of the anchor's row set after the
    /// `roots` predicate filter. Useful as a sanity check when
    /// `roots: Some(ids)` was passed: the count should equal `ids.len`
    /// minus any ids that did not exist in the source table.
    pub sources_visited: usize,
}

/// Marker trait that closure-model structs implement to surface the
/// column names the [`Model::materialize_closure`](crate::model::Model::materialize_closure)
/// SQL emitter needs.
/// # What it surfaces
/// Four columns the closure table must carry — the helper never
/// reaches inside the closure model to discover these because
/// macro-side wiring (`#[model(closure_for = T)]`) is out of scope
/// for B4. The trait shape is the runtime-stable contract a future
/// `closure_for` attribute would generate.
/// # Identifier validation
/// Every column name returned by this trait is identifier-validated
/// at helper-call time via [`crate::ident::check_user_supplied_ident`]:
/// ASCII alphabetic / underscore first byte, ASCII alphanumeric /
/// underscore remainder, ≤ 63 bytes, not a Postgres reserved keyword,
/// and not in the framework-reserved `__djogi_` prefix namespace. A
/// bad identifier surfaces as [`DjogiError::Validation`] before any
/// SQL is built — adopters cannot accidentally smuggle SQL through
/// the column-name accessors or shadow framework-internal aliases.
/// # `Source = T` binding
/// The associated `Source` type pins the closure model to its source
/// model at the type level. `T::materialize_closure::<C>` accepts only
/// closure models whose `C::Source = T` — wrong-source closure tables
/// fail at compile time.
pub trait ClosureModel: Model {
    /// The source model whose self-FK edges this closure walks.
    type Source: Model;

    /// SQL identifier of the closure table. Defaults to
    /// [`Model::table_name`] which the macro already validates;
    /// override only if your closure table sits behind a view or
    /// schema-qualified name.
    fn table() -> &'static str {
        Self::table_name()
    }

    /// Column name on the closure table holding the source row's id.
    /// Conventionally named `<source_singular>_id`, e.g.
    /// `elephant_id` for an `ElephantAncestry` closure of `Elephant`.
    fn source_column() -> &'static str;

    /// Column name on the closure table holding the ancestor's id.
    /// Conventionally `ancestor_id`.
    fn ancestor_column() -> &'static str;

    /// Column name on the closure table holding the recursive-walk
    /// depth (`INTEGER`, `0`-based — the source row itself is at
    /// depth `0`). Conventionally `depth`.
    fn depth_column() -> &'static str;

    /// Column name on the closure table holding the path-multiplicity
    /// count (`BIGINT`). Wright-style multiplicity: an ancestor
    /// reachable by two distinct edge sequences accumulates
    /// `path_count = 2`. Conventionally `path_count`.
    fn path_count_column() -> &'static str;
}

/// Build the closure SQL for `T` and `C` and execute it against `ctx`.
/// Routed through here (rather than the
/// [`Model::materialize_closure`](crate::model::Model::materialize_closure)
/// default method body) so the heavy SQL-building logic lives in one
/// place and the trait method is a thin generic delegate. Test
/// coverage in `#[cfg(test)]` exercises the pure SQL emitter
/// [`build_materialize_closure_sql`] directly.
pub(crate) fn materialize_closure_impl<'ctx, T, C>(
    ctx: &'ctx mut DjogiContext,
    opts: MaterializeClosureOptions<T::Pk>,
) -> impl Future<Output = Result<MaterializeClosureReport, DjogiError>> + Send + 'ctx
where
    T: Model,
    T::Pk: ToSql + Sync + Send + 'static,
    C: ClosureModel<Source = T>,
{
    async move {
        // Empty-roots short-circuit. `roots: Some(vec![])` means
        // "walk closure for no source rows" — we honour that by
        // returning a zeroed report rather than emitting `WHERE id
        // IN ` which Postgres rejects as a syntax error. Doing
        // this before identifier validation is intentional: even a
        // mis-configured closure model with bad column names
        // shouldn't fail when the caller asked for no work.
        if matches!(&opts.roots, Some(ids) if ids.is_empty()) {
            return Ok(MaterializeClosureReport {
                rows_written: 0,
                sources_visited: 0,
            });
        }

        // Reject self-FK-less source models with a descriptive
        // error before reaching the SQL builder. Mirrors B3's
        // `check_edges_present` contract for `full_ancestors`.
        let descriptor = T::descriptor();
        if descriptor.self_fk_count() == 0 {
            return Err(DjogiError::Validation(format!(
                "model '{}' has no self-FK; materialize_closure requires at least one",
                T::table_name(),
            )));
        }

        // Validate every closure-model column-name accessor up-front.
        // See [`validate_closure_metadata_idents`] for the contract.
        validate_closure_metadata_idents::<C>()?;

        auto_set_tenant::<T>(ctx).await?;

        let acc = build_materialize_closure_sql::<T, C>(opts);
        let (sql, binds) = acc.into_parts();
        let params = as_params(&binds);

        // The outer wrap returns exactly one row: `(rows_written: i64,
        // sources_visited: i64)` from the `COUNT(*)` /
        // `COUNT(DISTINCT)` aggregate. Both come back as Postgres
        // `BIGINT` (`i64`); the helper widens `rows_written` to `u64`
        // (always non-negative — `COUNT` cannot return negative
        // values) and narrows `sources_visited` to `usize` for ease
        // of comparison with the caller's `roots.len`.
        let row = ctx.query_one(&sql, &params).await?;
        let rows_written: i64 = try_get_scalar::<i64>(&row, 0)?;
        let sources_visited: i64 = try_get_scalar::<i64>(&row, 1)?;
        Ok(MaterializeClosureReport {
            rows_written: rows_written.max(0) as u64,
            sources_visited: sources_visited.max(0) as usize,
        })
    }
}

/// Validate that the closure-model `C`'s four column-name accessors
/// and table name satisfy the Postgres unquoted-identifier contract
/// AND the framework-reserved `__djogi_` prefix block.
/// Called by [`materialize_closure_impl`] and by
/// [`crate::query::joined::JoinedQuerySet::left_join_closure_pair`]
/// terminal paths — every code path that splices a closure model's
/// `&'static str` accessor return values into emitted SQL must run
/// these names through this gate first. Without it a typo, hostile
/// override, or hand-rolled `impl ClosureModel` could smuggle raw SQL
/// fragments through the `push_sql` sites the closure emitters call.
/// `check_user_supplied_ident(value, true)` enforces the four-rule
/// contract (Postgres unquoted-identifier shape, ≤ 63 bytes, not a
/// reserved keyword, not in the `__djogi_` framework-reserved prefix
/// namespace) for each name. See `docs/spec/reserved-identifiers.md`
/// for the policy that places adopter-provided closure accessor
/// strings under the "user-supplied identifier" contract — same as
/// window aliases, FTS dictionary names, and outbox table names.
/// # Errors
/// Returns [`DjogiError::Validation`] for the first failing identifier,
/// labelled with the role (`"closure table"`, `"source_column"`,
/// `"ancestor_column"`, `"depth_column"`, `"path_count_column"`) and
/// the underlying [`crate::ident::IdentError`].
pub(crate) fn validate_closure_metadata_idents<C: ClosureModel>() -> Result<(), DjogiError> {
    for (label, col) in [
        ("closure table", C::table()),
        ("source_column", C::source_column()),
        ("ancestor_column", C::ancestor_column()),
        ("depth_column", C::depth_column()),
        ("path_count_column", C::path_count_column()),
    ] {
        check_user_supplied_ident(col, true).map_err(|e| {
            DjogiError::Validation(format!(
                "ClosureModel<{}>::{} returned invalid identifier {:?}: {:?}",
                std::any::type_name::<C>(),
                label,
                col,
                e,
            ))
        })?;
    }
    Ok(())
}

/// Pure SQL emitter for the materialize-closure CTE — never touches a
/// connection. Returns the populated [`SqlAccumulator`] so unit tests
/// can assert on the emitted SQL shape and bind count without a live
/// database.
/// # Bind ordering
/// 1. `roots` ids (when `Some(ids)` with non-empty `ids`)
///    one bind slot per id, in caller-supplied order.
/// 2. `max_depth` — one bind slot total, attached to the WHERE on
///    the consolidated single recursive SELECT. Bound as `i32` to
///    match `closure.depth` (INTEGER / int4); `tokio_postgres`
///    requires exact bind/column type match. Edge count does not
///    affect this — every self-FK edge fans out through one
///    `CROSS JOIN LATERAL VALUES` clause, not per-edge UNION ALL
///    branches.
///    `tokio_postgres` re-receives bind values in the order the helper
///    pushes them; downstream readers should not assume `$1` is always
///    `max_depth` etc. The bind count returned by
///    [`SqlAccumulator::bind_count`] is the authoritative ordering.
pub(crate) fn build_materialize_closure_sql<T, C>(
    opts: MaterializeClosureOptions<T::Pk>,
) -> SqlAccumulator
where
    T: Model,
    T::Pk: ToSql + Sync + Send + 'static,
    C: ClosureModel<Source = T>,
{
    let mut acc = SqlAccumulator::new("");

    // ── Outer CTE wrap: WITH inserted AS (INSERT ... RETURNING ...) ───
    // Wrapping the INSERT inside a CTE lets the outer SELECT compute
    // both `rows_written` and `sources_visited` from the RETURNING
    // result set in one round trip. Without the wrap we would have
    // to either (a) issue a second query against the closure table,
    // or (b) pull all RETURNING rows back to Rust and count there
    // both worse than letting Postgres's aggregate planner do it.
    acc.push_sql("WITH inserted AS (INSERT INTO ");
    acc.push_sql(C::table());
    acc.push_sql(" (");
    acc.push_sql(C::source_column());
    acc.push_sql(", ");
    acc.push_sql(C::ancestor_column());
    acc.push_sql(", ");
    acc.push_sql(C::depth_column());
    acc.push_sql(", ");
    acc.push_sql(C::path_count_column());
    acc.push_sql(") ");

    // ── Recursive CTE: __djogi_closure ──────────────────────────────────
    // Column shape `(source_id, ancestor_id, depth, path)` — distinct
    // from the B2 / B3 `__djogi_tree` CTE's `(depth, path, <T cols>)`.
    // The closure table only ever needs the source id, the ancestor
    // id, the depth, and (for `GROUP BY`-driven multiplicity counting)
    // the edge-name path. T's other columns are not projected.
    acc.push_sql("WITH RECURSIVE __djogi_closure (source_id, ancestor_id, depth, path) AS (");

    // ── Anchor: every source row is its own ancestor at depth 0 ─────────
    // `s.id, s.id, 0, ARRAY[]::text[]` — the closure table records
    // self-pairs at depth 0 (Wright-style: `path_count = 1` for the
    // identity path). The anchor's `WHERE` either selects every
    // source row (`TRUE`) or restricts to the explicit `roots` Vec
    // via `s.id IN ($a, $b, ...)`.
    acc.push_sql("SELECT s.id, s.id, 0, ARRAY[]::text[] FROM ");
    acc.push_sql(T::table_name());
    acc.push_sql(" s WHERE ");
    // Move `roots` out of `opts` so we can consume the Vec into bind
    // slots without re-borrowing `opts` later.
    let roots = opts.roots;
    let max_depth = opts.max_depth;
    match roots {
        None => {
            // Unbounded mode — anchor matches every source row. `TRUE`
            // is a stable predicate the planner short-circuits.
            acc.push_sql("TRUE");
        }
        Some(ids) => {
            // Bounded mode — `s.id IN ($n, $n+1, ...)`. Empty Vec was
            // short-circuited in `materialize_closure_impl` before
            // reaching the SQL emitter, so we always emit at least
            // one bind slot here.
            debug_assert!(
                !ids.is_empty(),
                "empty roots Vec must be short-circuited before SQL emission",
            );
            acc.push_sql("s.id IN (");
            acc.push_list_binds(ids);
            acc.push_sql(")");
        }
    }

    // ── UNION ALL — single recursive term, all self-FK edges fanned out ──
    // Walks ANCESTORS direction. The closure CTE only carries
    // `(source_id, ancestor_id, depth, path)` — it does NOT carry the
    // self-FK columns of `T`. So to step from "current ancestor" to
    // "ancestor's parent", we join the source table twice: first to
    // resolve the current `ancestor_id` back to its full row (`a`),
    // then to follow `a.<edge_col>` up to the next ancestor (`p`).
    // `p.id` becomes the new `ancestor_id` for the next layer.
    // **Single recursive reference invariant.** Postgres restricts
    // recursive CTEs to ONE self-reference in the recursive term
    // (rejecting both "non-recursive term contains a recursive
    // reference" and "recursive reference must not appear more than
    // once" forms). For multi-edge models we therefore enumerate
    // edges via `CROSS JOIN LATERAL (VALUES ...) AS step(pid, label)`
    // and join `p` against `step.pid` once — fanning out one row per
    // (closure-row, edge) pair while keeping the SELF-reference
    // count at exactly 1. NULL-valued edge columns get filtered out
    // by the inner `JOIN T p ON p.id = step.pid` since `NULL = ...`
    // is unknown. Multi-path multiplicity is preserved because each
    // edge that fires emits its own row, which the outer
    // `GROUP BY (source, ancestor, depth)` collapses to
    // `path_count = COUNT(*)`.
    let edges: Vec<&'static str> = T::descriptor().self_fk_columns().collect();
    acc.push_sql(
        " UNION ALL SELECT closure.source_id, p.id, closure.depth + 1, \
         closure.path || ARRAY[step.label] FROM __djogi_closure closure JOIN ",
    );
    acc.push_sql(T::table_name());
    acc.push_sql(" a ON a.id = closure.ancestor_id CROSS JOIN LATERAL (VALUES ");
    for (i, edge) in edges.iter().enumerate() {
        if i > 0 {
            acc.push_sql(", ");
        }
        acc.push_sql("(a.");
        // `edge` came from `descriptor.self_fk_columns` which
        // surfaces the field name verbatim — already
        // identifier-validated at macro emission.
        acc.push_sql(edge);
        acc.push_sql(", '");
        acc.push_sql(edge);
        acc.push_sql("'::text)");
    }
    acc.push_sql(") AS step(pid, label) JOIN ");
    acc.push_sql(T::table_name());
    acc.push_sql(" p ON p.id = step.pid");
    if let Some(n) = max_depth {
        acc.push_sql(" WHERE closure.depth < ");
        // `closure.depth` is INTEGER (int4) in Postgres — bind as
        // `i32` (not `i64`) so `tokio_postgres` accepts the encoding.
        // `WrongType { postgres: Int4, rust: "i64" }` is a hard
        // protocol-level failure, not a coercion warning. `u32 →
        // i32` is well-defined for u32 ≤ i32::MAX (saturating clamp
        // for the unrealistic case of u32 > i32::MAX, which would
        // mean a recursion-depth cap above ~2 billion — far past
        // any sensible value).
        let n_i32 = i32::try_from(n).unwrap_or(i32::MAX);
        acc.push_bind(n_i32);
    }

    // ──) CYCLE source_id, ancestor_id SET is_cycle USING cycle_path ────
    // **Two-column** cycle key — closure walks from N source rows
    // simultaneously, so the cycle-detection key is the
    // `(source, ancestor)` pair, not just `ancestor`. A cycle is
    // "this source revisited this ancestor"; reaching the same
    // ancestor from a *different* source is correct closure
    // expansion, not a cycle.
    // `cycle_path` (not `path`) frees `path` for our user-visible
    // edge-name accumulator — same contract as B3's CTE rename.
    acc.push_sql(") CYCLE source_id, ancestor_id SET is_cycle USING cycle_path");

    // ── Outer SELECT: GROUP BY (source_id, ancestor_id, depth) ──────────
    // `COUNT(*)` collapses the per-path rows the recursive term
    // emitted into one row per `(source, ancestor, depth)` triple,
    // surfacing the path multiplicity as `path_count`. Wright
    // kinship sums coefficients across distinct paths to the same
    // ancestor, so this aggregate is load-bearing — without it,
    // multi-path ancestors would be `INSERT`'d N times and the
    // `ON CONFLICT` deduplication would silently drop the
    // multiplicity information.
    acc.push_sql(
        " SELECT source_id, ancestor_id, depth, COUNT(*) AS path_count \
         FROM __djogi_closure WHERE NOT is_cycle \
         GROUP BY source_id, ancestor_id, depth",
    );

    // ── ON CONFLICT (...) DO UPDATE SET <col> = EXCLUDED.<col> ──────────
    // Replace, not add. Each `materialize_closure` invocation walks the
    // current graph from scratch via the recursive CTE, so EXCLUDED's
    // `path_count` is already the *correct, current* total of distinct
    // paths between every `(source, ancestor, depth)` triple in the
    // present graph state. The pre-existing closure row's value is the
    // previous (possibly stale) total; replacing it with EXCLUDED keeps
    // the closure aligned with whatever is in the source table now.
    // Additive merge would be wrong on every callsite that matters: a
    // straight rerun would double, an incremental rerun (after new edges
    // are added in the source table) would over-count because the
    // recursive walk re-derives every existing path on top of the new
    // ones. The `additive` shape only makes sense when the closure is
    // updated by some external partial-write process that hands djogi
    // a delta — that is not djogi's design.
    // The unique constraint `(source, ancestor, depth)` is required on
    // the closure table — see module docs.
    acc.push_sql(" ON CONFLICT (");
    acc.push_sql(C::source_column());
    acc.push_sql(", ");
    acc.push_sql(C::ancestor_column());
    acc.push_sql(", ");
    acc.push_sql(C::depth_column());
    acc.push_sql(") DO UPDATE SET ");
    acc.push_sql(C::path_count_column());
    acc.push_sql(" = EXCLUDED.");
    acc.push_sql(C::path_count_column());
    acc.push_sql(" RETURNING ");
    acc.push_sql(C::source_column());

    // ── Outer wrap: SELECT COUNT(*), COUNT(DISTINCT source) FROM inserted
    acc.push_sql(") SELECT COUNT(*), COUNT(DISTINCT ");
    acc.push_sql(C::source_column());
    acc.push_sql(") FROM inserted");

    acc
}

#[cfg(test)]
mod tests {
    //! Pure SQL-builder tests — every assertion is a string check on
    //! the emitted SQL or a count of bind slots. No live database is
    //! reached here; integration tests against a real Postgres live
    //! in B5.
    //! `MiniTree` is the same stub model the recursive-CTE tests use,
    //! reusing its `Model` + `FromPgRow` impls would create a circular
    //! `cfg(test)` dependency — instead we declare a fresh stub here
    //! that pairs with `MiniClosure`, an in-test `ClosureModel` impl.

    use super::*;
    use crate::descriptor::{
        FieldDescriptor, FieldSqlType, ModelDescriptor, PkType, RelationKind, field_descriptor,
        model_descriptor,
    };
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
            // One self-FK edge: `parent_id`. Matches the
            // single-edge baseline tests in `recursive::tests`.
            &MINI_TREE_DESC_ONE_EDGE
        }
        fn get(
            _ctx: &mut DjogiContext,
            _id: Self::Pk,
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
    impl FromPgRow for MiniTree {
        const COLUMNS: &'static [&'static str] = &["id", "parent_id"];
        const COLUMN_LIST: &'static str = "id, parent_id";
        fn from_pg_row(_row: &tokio_postgres::Row) -> Result<Self, DjogiError> {
            unreachable!()
        }
    }

    /// Two-edge variant — pairs with the `mother_id` / `father_id`
    /// scenario from `recursive::tests`. Wraps `MiniTree` in a
    /// newtype so we can give it a separate descriptor without
    /// rewiring the single-edge baseline.
    struct MiniPedigree;
    impl crate::model::__sealed::Sealed for MiniPedigree {}
    impl crate::model::Model for MiniPedigree {
        type Pk = HeerId;
        type Fields = ();
        fn table_name() -> &'static str {
            "mini_pedigrees"
        }
        fn pk_value(&self) -> &Self::Pk {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            &MINI_PEDIGREE_DESC_TWO_EDGES
        }
        fn get(
            _ctx: &mut DjogiContext,
            _id: Self::Pk,
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
    impl FromPgRow for MiniPedigree {
        const COLUMNS: &'static [&'static str] = &["id", "mother_id", "father_id"];
        const COLUMN_LIST: &'static str = "id, mother_id, father_id";
        fn from_pg_row(_row: &tokio_postgres::Row) -> Result<Self, DjogiError> {
            unreachable!()
        }
    }

    /// Closure model for `MiniTree` — names the four columns the
    /// emitter splices into the INSERT / ON CONFLICT clauses.
    struct MiniClosure;
    impl crate::model::__sealed::Sealed for MiniClosure {}
    impl crate::model::Model for MiniClosure {
        type Pk = HeerId;
        type Fields = ();
        fn table_name() -> &'static str {
            "mini_tree_closures"
        }
        fn pk_value(&self) -> &Self::Pk {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            unreachable!()
        }
        fn get(
            _ctx: &mut DjogiContext,
            _id: Self::Pk,
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
    impl FromPgRow for MiniClosure {
        const COLUMNS: &'static [&'static str] = &[];
        const COLUMN_LIST: &'static str = "";
        fn from_pg_row(_row: &tokio_postgres::Row) -> Result<Self, DjogiError> {
            unreachable!()
        }
    }
    impl ClosureModel for MiniClosure {
        type Source = MiniTree;
        fn source_column() -> &'static str {
            "mini_tree_id"
        }
        fn ancestor_column() -> &'static str {
            "ancestor_id"
        }
        fn depth_column() -> &'static str {
            "depth"
        }
        fn path_count_column() -> &'static str {
            "path_count"
        }
    }

    /// Closure model for `MiniPedigree` — distinct table to verify
    /// the consolidated single-recursive-term emission against a
    /// real two-edge descriptor (both edges fan out via the
    /// `CROSS JOIN LATERAL VALUES` clause inside one recursive
    /// SELECT).
    struct MiniPedigreeClosure;
    impl crate::model::__sealed::Sealed for MiniPedigreeClosure {}
    impl crate::model::Model for MiniPedigreeClosure {
        type Pk = HeerId;
        type Fields = ();
        fn table_name() -> &'static str {
            "mini_pedigree_closures"
        }
        fn pk_value(&self) -> &Self::Pk {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            unreachable!()
        }
        fn get(
            _ctx: &mut DjogiContext,
            _id: Self::Pk,
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
    impl FromPgRow for MiniPedigreeClosure {
        const COLUMNS: &'static [&'static str] = &[];
        const COLUMN_LIST: &'static str = "";
        fn from_pg_row(_row: &tokio_postgres::Row) -> Result<Self, DjogiError> {
            unreachable!()
        }
    }
    impl ClosureModel for MiniPedigreeClosure {
        type Source = MiniPedigree;
        fn source_column() -> &'static str {
            "mini_pedigree_id"
        }
        fn ancestor_column() -> &'static str {
            "ancestor_id"
        }
        fn depth_column() -> &'static str {
            "depth"
        }
        fn path_count_column() -> &'static str {
            "path_count"
        }
    }

    /// Build a minimal `FieldDescriptor` for a self-FK column
    /// the closure builder reads `name`, `relation_kind`, and
    /// `is_self_fk`; everything else is defaulted via
    /// [`field_descriptor`].
    const fn self_fk_field(name: &'static str) -> FieldDescriptor {
        FieldDescriptor {
            relation_kind: Some(RelationKind::ForeignKey),
            is_self_fk: true,
            ..field_descriptor(name, FieldSqlType::BigInt, false)
        }
    }

    static MINI_TREE_FIELDS: &[FieldDescriptor] = &[self_fk_field("parent_id")];
    static MINI_TREE_DESC_ONE_EDGE: ModelDescriptor =
        model_descriptor("MiniTree", "mini_trees", PkType::HeerId, MINI_TREE_FIELDS);

    static MINI_PEDIGREE_FIELDS: &[FieldDescriptor] =
        &[self_fk_field("mother_id"), self_fk_field("father_id")];
    static MINI_PEDIGREE_DESC_TWO_EDGES: ModelDescriptor = model_descriptor(
        "MiniPedigree",
        "mini_pedigrees",
        PkType::HeerId,
        MINI_PEDIGREE_FIELDS,
    );

    fn opts_default() -> MaterializeClosureOptions<HeerId> {
        MaterializeClosureOptions::<HeerId>::default()
    }

    #[test]
    fn anchor_projects_source_ancestor_depth_path() {
        // Anchor row shape: `(source_id, ancestor_id, depth, path)`
        // where source == ancestor at depth 0 and path is the empty
        // text array. The closure CTE column shape is fundamentally
        // different from the recursive-CTE shape (`(depth, path,
        // <T cols>)`) — this assertion pins it.
        let acc = build_materialize_closure_sql::<MiniTree, MiniClosure>(opts_default());
        let sql = acc.sql();
        assert!(
            sql.contains("__djogi_closure (source_id, ancestor_id, depth, path)"),
            "CTE column list must be (source_id, ancestor_id, depth, path): {sql}"
        );
        assert!(
            sql.contains("SELECT s.id, s.id, 0, ARRAY[]::text[] FROM mini_trees s"),
            "anchor must project (s.id, s.id, 0, empty path): {sql}"
        );
    }

    #[test]
    fn one_edge_emits_single_recursive_term() {
        let acc = build_materialize_closure_sql::<MiniTree, MiniClosure>(opts_default());
        let sql = acc.sql();
        // anchor + one recursive term = exactly one UNION ALL.
        let union_count = sql.matches("UNION ALL").count();
        assert_eq!(
            union_count, 1,
            "single-edge closure: exactly 1 UNION ALL (anchor + recursive): {sql}"
        );
        // ANCESTORS direction: closure CTE only carries `(source_id,
        // ancestor_id, depth, path)`, so we double-join the source
        // table — once to resolve the current ancestor row (`a`),
        // once via `step.pid` to the new ancestor (`p`).
        assert!(
            sql.contains("a ON a.id = closure.ancestor_id"),
            "must resolve ancestor row via closure.ancestor_id: {sql}"
        );
        assert!(
            sql.contains("(a.parent_id, 'parent_id'::text)"),
            "single-edge LATERAL VALUES tuple must contain (a.parent_id, 'parent_id'::text): {sql}"
        );
        assert!(
            sql.contains("p ON p.id = step.pid"),
            "must step to parent via the LATERAL `step.pid`: {sql}"
        );
    }

    #[test]
    fn two_edges_collapse_into_single_recursive_term() {
        // Postgres restricts recursive CTEs to ONE self-reference in
        // the recursive term. Multi-edge models therefore enumerate
        // edges via CROSS JOIN LATERAL VALUES rather than per-edge
        // UNION ALL branches. The per-edge form would fail with "recursive
        // reference must not appear more than once".
        let opts = MaterializeClosureOptions::<HeerId>::default();
        let acc = build_materialize_closure_sql::<MiniPedigree, MiniPedigreeClosure>(opts);
        let sql = acc.sql();
        let union_count = sql.matches("UNION ALL").count();
        assert_eq!(
            union_count, 1,
            "two-edge closure: exactly 1 UNION ALL even with N edges (anchor + single recursive term): {sql}"
        );
        // Both edges live inside the single LATERAL VALUES tuple.
        assert!(
            sql.contains("(a.mother_id, 'mother_id'::text), (a.father_id, 'father_id'::text)"),
            "LATERAL VALUES must enumerate both edges as (a.<col>, '<col>'::text) tuples: {sql}"
        );
        assert!(
            sql.contains("p ON p.id = step.pid"),
            "must step to parent via LATERAL `step.pid` — single JOIN, not per-edge: {sql}"
        );
    }

    #[test]
    fn cycle_clause_uses_two_columns() {
        // Closure walks from N source rows simultaneously — cycle
        // detection key is (source_id, ancestor_id), not just
        // ancestor_id. `cycle_path` (not `path`) frees `path` for
        // the user-visible edge-name accumulator.
        let acc = build_materialize_closure_sql::<MiniTree, MiniClosure>(opts_default());
        let sql = acc.sql();
        assert!(
            sql.contains("CYCLE source_id, ancestor_id SET is_cycle USING cycle_path"),
            "CYCLE clause must use (source_id, ancestor_id) and cycle_path array: {sql}"
        );
    }

    #[test]
    fn outer_select_groups_by_source_ancestor_depth() {
        // Wright multiplicity: COUNT(*) over the per-path rows in
        // the recursive term, grouped by (source, ancestor, depth)
        // surfaces the path multiplicity as path_count. Without
        // this aggregate the ON CONFLICT would silently drop
        // multi-path information.
        let acc = build_materialize_closure_sql::<MiniTree, MiniClosure>(opts_default());
        let sql = acc.sql();
        assert!(
            sql.contains("SELECT source_id, ancestor_id, depth, COUNT(*) AS path_count"),
            "outer SELECT must aggregate to (source, ancestor, depth, COUNT(*)): {sql}"
        );
        assert!(
            sql.contains("GROUP BY source_id, ancestor_id, depth"),
            "outer SELECT must GROUP BY (source, ancestor, depth): {sql}"
        );
    }

    #[test]
    fn on_conflict_clause_replaces_path_count() {
        // ON CONFLICT (...) DO UPDATE SET path_count = EXCLUDED.path_count
        // replace, not add. Each invocation walks the current graph
        // from scratch via the recursive CTE, so EXCLUDED's path_count
        // is already the correct total. Additive merge would double on
        // straight rerun and over-count on incremental rerun.
        let acc = build_materialize_closure_sql::<MiniTree, MiniClosure>(opts_default());
        let sql = acc.sql();
        assert!(
            sql.contains(
                "ON CONFLICT (mini_tree_id, ancestor_id, depth) DO UPDATE SET path_count = EXCLUDED.path_count"
            ),
            "ON CONFLICT clause must replace path_count from EXCLUDED: {sql}"
        );
        assert!(
            !sql.contains("mini_tree_closures.path_count + EXCLUDED.path_count"),
            "additive merge must not appear (would double on rerun): {sql}"
        );
    }

    #[test]
    fn returning_source_col_drives_outer_count() {
        // The CTE wrap pattern: INSERT ... RETURNING <source_col>
        // feeds a CTE named `inserted`; the outer SELECT computes
        // (COUNT(*), COUNT(DISTINCT <source_col>)) in a single round
        // trip. Verifies both halves are present.
        let acc = build_materialize_closure_sql::<MiniTree, MiniClosure>(opts_default());
        let sql = acc.sql();
        assert!(
            sql.starts_with("WITH inserted AS (INSERT INTO mini_tree_closures"),
            "outer wrap must start with WITH inserted AS (INSERT...): {sql}"
        );
        assert!(
            sql.contains("RETURNING mini_tree_id"),
            "INSERT must RETURN the source_col so the outer COUNT can run on it: {sql}"
        );
        assert!(
            sql.ends_with(") SELECT COUNT(*), COUNT(DISTINCT mini_tree_id) FROM inserted"),
            "outer SELECT must compute (rows_written, sources_visited): {sql}"
        );
    }

    #[test]
    fn no_max_depth_no_predicate() {
        let acc = build_materialize_closure_sql::<MiniTree, MiniClosure>(opts_default());
        let sql = acc.sql();
        assert!(
            !sql.contains("closure.depth <"),
            "no max_depth must not emit `closure.depth <`: {sql}"
        );
    }

    #[test]
    fn with_max_depth_emits_depth_predicate_and_binds() {
        let opts = MaterializeClosureOptions::<HeerId>::default().with_max_depth(5);
        let acc = build_materialize_closure_sql::<MiniTree, MiniClosure>(opts);
        let sql = acc.sql();
        assert!(
            sql.contains("closure.depth < $1"),
            "max_depth must bind as $1 (no roots binds): {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            1,
            "single-edge + max_depth = 1 bind, got {}",
            acc.bind_count()
        );
    }

    #[test]
    fn two_edges_max_depth_binds_once() {
        // The recursive term is now a single SELECT (per Postgres's
        // "exactly one self-reference" rule) that fans edges out via
        // CROSS JOIN LATERAL VALUES. The depth-cap WHERE attaches to
        // that single SELECT, so multi-edge models emit one
        // max_depth bind regardless of edge count.
        let opts = MaterializeClosureOptions::<HeerId>::default().with_max_depth(3);
        let acc = build_materialize_closure_sql::<MiniPedigree, MiniPedigreeClosure>(opts);
        let sql = acc.sql();
        assert!(
            sql.contains("closure.depth < $1"),
            "single recursive term => single depth bind at $1: {sql}"
        );
        assert!(
            !sql.contains("closure.depth < $2"),
            "second depth bind must not appear — only one recursive term: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            1,
            "consolidated recursive term => 1 max_depth bind regardless of edge count, got {}",
            acc.bind_count()
        );
    }

    #[test]
    fn no_roots_emits_where_true() {
        let acc = build_materialize_closure_sql::<MiniTree, MiniClosure>(opts_default());
        let sql = acc.sql();
        assert!(
            sql.contains("FROM mini_trees s WHERE TRUE"),
            "no roots must emit WHERE TRUE: {sql}"
        );
    }

    #[test]
    fn with_roots_emits_in_clause_and_binds_each_id() {
        let ids = vec![
            HeerId::from_i64(1).unwrap(),
            HeerId::from_i64(2).unwrap(),
            HeerId::from_i64(3).unwrap(),
        ];
        let opts = MaterializeClosureOptions::<HeerId>::default().with_roots(ids);
        let acc = build_materialize_closure_sql::<MiniTree, MiniClosure>(opts);
        let sql = acc.sql();
        assert!(
            sql.contains("FROM mini_trees s WHERE s.id IN ($1, $2, $3)"),
            "with_roots must emit s.id IN (<binds>) with one bind per id: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            3,
            "three roots = 3 binds, got {}",
            acc.bind_count()
        );
        // No max_depth → no extra bind slots beyond the roots list.
        assert!(
            !sql.contains("closure.depth <"),
            "no max_depth must not emit a depth probe: {sql}"
        );
    }

    #[test]
    fn with_roots_and_max_depth_binds_after_roots() {
        // Bind ordering: roots first, then `max_depth` (one bind
        // total — attached to the WHERE on the consolidated single
        // recursive SELECT, regardless of edge count). `$1..$3` are
        // the three roots; `$4` is the depth cap.
        let ids = vec![
            HeerId::from_i64(1).unwrap(),
            HeerId::from_i64(2).unwrap(),
            HeerId::from_i64(3).unwrap(),
        ];
        let opts = MaterializeClosureOptions::<HeerId>::default()
            .with_roots(ids)
            .with_max_depth(7);
        let acc = build_materialize_closure_sql::<MiniTree, MiniClosure>(opts);
        let sql = acc.sql();
        assert!(
            sql.contains("s.id IN ($1, $2, $3)"),
            "roots take $1..$3: {sql}"
        );
        assert!(
            sql.contains("closure.depth < $4"),
            "max_depth bind starts at $4 (after the 3 roots): {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            4,
            "3 roots + 1 depth cap = 4 binds, got {}",
            acc.bind_count()
        );
    }

    #[test]
    fn empty_edges_descriptor_reports_zero_count() {
        // Sanity: a descriptor with no self-FK fields reports
        // `self_fk_count == 0`. Exercised in the impl path via
        // `materialize_closure_impl` (which errors on this case);
        // here we pin the descriptor-level invariant the impl
        // relies on.
        static NO_FIELDS: &[FieldDescriptor] = &[];
        static MINI_DESC_NO_EDGES: ModelDescriptor =
            model_descriptor("MiniNoEdges", "mini_no_edges", PkType::HeerId, NO_FIELDS);
        assert_eq!(MINI_DESC_NO_EDGES.self_fk_count(), 0);
    }
}
