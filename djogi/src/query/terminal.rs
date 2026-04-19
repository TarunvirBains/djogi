//! Terminal read methods on [`QuerySet<T>`].
//!
//! # What
//!
//! Every method here is a terminal — it consumes the queryset, executes SQL
//! against a caller-provided `sqlx::Executor`, and returns a decoded result
//! (`Vec<T>`, `T`, `Option<T>`, `i64`, `bool`). This is the **only** place
//! in the query layer that talks to the database.
//!
//! # Why kept in its own file
//!
//! Splitting read terminals out of `queryset.rs` keeps each file auditable:
//! the builder file deals only with structural transforms (filter/order/limit
//! accumulation), this file owns SQL execution + error mapping. Writes
//! (`update` / `delete`) land in a sibling `update.rs` module in a later
//! task rather than being mixed into this file. Each layer can be reviewed
//! for its own invariants (no SQL in queryset.rs; no mutation of accumulated
//! state in terminal.rs beyond the documented `limit` override in
//! `fetch_one` / `first`).
//!
//! # Executor generics and `Send` futures
//!
//! Each terminal takes `impl sqlx::Executor<'a, Database = sqlx::Postgres>`,
//! matching the pattern established by the Phase 1 `Model` trait methods:
//! the same call site works against a `&PgPool` (auto-connection) or a
//! `&mut *tx` (transaction). Returning `impl Future<Output = ...> + Send`
//! (RPITIT) means callers can `.await` results across task boundaries —
//! required for Axum handlers that run on the multi-thread runtime.
//!
//! # `is_empty` short-circuit contract
//!
//! Every terminal honours the `QuerySet::none()` contract — a queryset
//! marked `is_empty = true` returns the empty result **without issuing any
//! SQL**:
//!
//! | Method       | Empty result                                   |
//! |--------------|------------------------------------------------|
//! | `fetch_all`  | `Ok(vec![])`                                   |
//! | `fetch_one`  | `Err(DjogiError::NotFound { table: T::... })`  |
//! | `first`      | `Ok(None)`                                     |
//! | `count`      | `Ok(0)`                                        |
//! | `exists`     | `Ok(false)`                                    |
//!
//! The grep marker `TASK6:empty_contract` on the `is_empty` field in
//! `queryset.rs` is the anchor for this contract — if the field shape ever
//! changes, that marker surfaces every terminal that needs updating.
//!
//! # `fetch_one` row-count strategy
//!
//! `fetch_one` expects **exactly one** row. Rather than issuing two round
//! trips (`COUNT(*)` then `SELECT`), it rewrites the user's `limit` to 2
//! before building the SELECT. A single-element result is success; a
//! two-element result proves "more than one matches" without scanning the
//! whole table, which matters for unbounded-cardinality filters (e.g.
//! `published = true` on a posts table). Zero rows -> `NotFound`.
//!
//! # Why RPITIT (not `async fn`)
//!
//! Every terminal returns `impl Future<Output = ...> + Send` rather than
//! using bare `async fn`. The explicit `+ Send` bound matches the Phase 1
//! `Model` trait shape (`model.rs`) and guarantees the returned future can
//! be `.await`ed across task boundaries — critical for Axum handlers on the
//! multi-thread runtime. `async fn` in trait / impl position does not
//! automatically carry `+ Send` on the returned future; spelling the bound
//! explicitly keeps the call site free of "future is not Send" errors that
//! would otherwise show up far from where the bound is missing.
//!
//! `clippy::manual_async_fn` fires on this pattern; the lint is allowed at
//! the module level because the explicit-bound form is the deliberate
//! choice, not an oversight.
#![allow(clippy::manual_async_fn)]

use crate::DjogiError;
use crate::model::Model;
use crate::query::queryset::QuerySet;
use crate::query::sql::{build_count, build_exists, build_select, build_select_joined};
use crate::relation::joined_row::{FromJoinedRow, JoinedRow};
use crate::relation::prefetch::{PrefetchedRow, apply_prefetches};
use crate::relation::select_related::{apply_select_related, stitch_prefetches_into_joined};
use std::future::Future;

// ── Row-returning terminals (require `T: FromRow`) ────────────────────────
//
// `fetch_all` / `fetch_one` / `first` decode rows into `T`, so they need
// `T: for<'r> FromRow<'r, PgRow>`. `count` / `exists` return scalars and
// are in a separate impl block below with no FromRow bound.

impl<T: Model> QuerySet<T>
where
    T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> + Send + Unpin,
{
    /// Execute the query and collect every matching row into a `Vec<T>`.
    ///
    /// A `QuerySet::none()`-derived queryset returns `Ok(Vec::new())` without
    /// touching the database — see the module docs for the full short-
    /// circuit contract.
    pub fn fetch_all<'a, E>(
        self,
        executor: E,
    ) -> impl Future<Output = Result<Vec<T>, DjogiError>> + Send
    where
        E: sqlx::Executor<'a, Database = sqlx::Postgres> + Send,
    {
        async move {
            // TASK6:empty_contract — structural-none queryset, no SQL.
            if self.is_empty() {
                return Ok(Vec::new());
            }
            let mut qb = build_select(&self);
            let rows: Vec<T> = qb
                .build_query_as::<T>()
                .fetch_all(executor)
                .await
                .map_err(DjogiError::from)?;
            Ok(rows)
        }
    }

    /// Execute the query and require **exactly one** matching row.
    ///
    /// - Zero rows -> [`DjogiError::NotFound`].
    /// - Two or more rows -> [`DjogiError::MultipleObjects`] (via `LIMIT 2`;
    ///   `count_seen = 2`).
    ///
    /// User-supplied `limit` on the queryset is ignored — this terminal
    /// owns the row-count probe.
    pub fn fetch_one<'a, E>(self, executor: E) -> impl Future<Output = Result<T, DjogiError>> + Send
    where
        E: sqlx::Executor<'a, Database = sqlx::Postgres> + Send,
    {
        async move {
            // TASK6:empty_contract — structural-none treats as "no row
            // matched" on purpose: the caller asked for exactly-one and
            // got zero, which is precisely what `NotFound` means.
            if self.is_empty() {
                return Err(DjogiError::not_found(T::table_name()));
            }
            // Override the user's LIMIT (if any) with 2 so we can
            // distinguish the single-row success path from the
            // multiple-rows error path without a `COUNT(*)` round trip.
            let mut qs = self;
            qs.limit = Some(2);
            let mut qb = build_select(&qs);
            let rows: Vec<T> = qb
                .build_query_as::<T>()
                .fetch_all(executor)
                .await
                .map_err(DjogiError::from)?;
            match rows.len() {
                0 => Err(DjogiError::not_found(T::table_name())),
                1 => {
                    // `into_iter().next().unwrap()` is safe — we just
                    // matched `len() == 1`. `expect` instead of `unwrap`
                    // keeps the message actionable in the unlikely event a
                    // future refactor reshapes the branch.
                    let row = rows
                        .into_iter()
                        .next()
                        .expect("rows.len() == 1 was just matched");
                    Ok(row)
                }
                // `n` here is a sentinel: because we force `LIMIT 2`
                // above, `n` is always exactly 2 on this branch — not the
                // true matching-row count. The cap is intentional (one
                // round trip instead of a `COUNT(*)` probe); callers who
                // need the precise count follow up with `.count()`. Ship
                // the sentinel through to `MultipleObjects.count_seen`; the
                // error message renders "at least 2".
                n => Err(DjogiError::multiple_objects(T::table_name(), n)),
            }
        }
    }

    /// Execute the query and collect every matching row into a
    /// `Vec<PrefetchedRow<T>>`, materialising every relation registered
    /// via [`QuerySet::prefetch`](crate::query::QuerySet::prefetch) in
    /// follow-up SQL queries and stitching the results back into per-row
    /// wrappers.
    ///
    /// # Why a separate terminal (not a change to `fetch_all`)
    ///
    /// Preserving [`fetch_all`](Self::fetch_all)'s `Vec<T>` return type
    /// keeps the Phase 2 terminal stable across Phase 3: a queryset
    /// built without prefetches and fetched via `fetch_all` returns
    /// exactly what it did before Task 4 landed. Prefetches are an
    /// opt-in extension reachable through the dedicated
    /// `fetch_all_prefetched` entry — no pre-existing call site is
    /// forced into `Vec<PrefetchedRow<T>>`. This also makes prefetch
    /// registrations free on querysets whose terminal happens to be
    /// `fetch_all`: the `prefetch_paths` field is ignored on that path,
    /// which is documented on the field itself.
    ///
    /// # Short-circuit contract
    ///
    /// Honours the same `is_empty` short-circuit as the other terminals:
    /// a structural-none queryset returns `Ok(Vec::new())` without
    /// touching the database. An empty main result also short-circuits
    /// the prefetch pass — no prefetch loader runs when there are no
    /// parent rows to stitch against.
    ///
    /// # Executor shape
    ///
    /// Takes `&PgPool` concretely rather than the `impl Executor`
    /// generic that [`fetch_all`](Self::fetch_all) accepts. The
    /// prefetch loader fan-out runs *after* the main query completes;
    /// keeping the pool reference lets every loader grab its own
    /// connection from the pool without passing ownership or threading
    /// lifetimes through the type-erased [`ErasedPrefetch`](
    /// crate::relation::prefetch::ErasedPrefetch) fn-pointer signature.
    /// A `&mut Transaction` executor overload lands later if the
    /// shell / admin paths need prefetch inside a single transactional
    /// scope; Phase 3 ships pool-only.
    pub fn fetch_all_prefetched(
        self,
        pool: &sqlx::PgPool,
    ) -> impl Future<Output = Result<Vec<PrefetchedRow<T>>, DjogiError>> + Send + '_
    where
        T::Pk: Clone + Send + Sync + 'static,
    {
        async move {
            // TASK6:empty_contract — structural-none queryset returns
            // `Vec::new()` without touching the DB. Mirrors the
            // `fetch_all` short-circuit.
            if self.is_empty() {
                return Ok(Vec::new());
            }

            // Snapshot prefetch paths before we consume `self` in the
            // main-query build. The shared SQL emitter borrows the
            // queryset, so we pull out what we need first.
            let prefetches = self.prefetch_paths.clone();

            // Main query — identical shape to `fetch_all`.
            let mut qb = build_select(&self);
            let rows: Vec<T> = qb
                .build_query_as::<T>()
                .fetch_all(pool)
                .await
                .map_err(DjogiError::from)?;

            // Apply each prefetch loader. Empty main result -> no
            // loaders run (short-circuit inside `apply_prefetches`).
            apply_prefetches(pool, &prefetches, rows).await
        }
    }

    /// Execute the query as a single `LEFT JOIN` per registered
    /// select_related path and collect every matching row into a
    /// `Vec<JoinedRow<T>>`, with joined child rows exposed via
    /// [`JoinedRow::get`](crate::relation::JoinedRow::get).
    ///
    /// # Why a separate terminal (not a change to `fetch_all`)
    ///
    /// Preserving [`fetch_all`](Self::fetch_all)'s `Vec<T>` return
    /// type keeps the Phase 2 terminal stable across Phase 3: a
    /// queryset built without select_related and fetched via
    /// `fetch_all` returns exactly what it did before Task 5 landed.
    /// select_related is an opt-in extension reachable through the
    /// dedicated `fetch_all_joined` entry — no pre-existing call site
    /// is forced into `Vec<JoinedRow<T>>`. Registrations on the
    /// `select_related_paths` queryset field are ignored on the plain
    /// `fetch_all` path, matching the free-register-on-any-terminal
    /// behaviour `prefetch` already documents.
    ///
    /// # Composes with `.prefetch(...)`
    ///
    /// When the queryset carries both select_related and prefetch
    /// registrations, the terminal runs:
    ///
    /// 1. The main query with the LEFT JOINs + aliased child columns.
    ///    Each row decodes into a `JoinedRow<T>` carrying the joined
    ///    children under their `source_column` keys.
    /// 2. The prefetch fan-out — one follow-up query per registered
    ///    `prefetch_paths` entry — whose resolved targets are
    ///    stitched into the same `JoinedRow<T>` values. The two
    ///    paths never collide on the same `source_column` in practice
    ///    because `.select_related(path)` and `.prefetch(path)`
    ///    target different relations on any realistic queryset, but
    ///    if they did, the prefetch stitcher would overwrite the
    ///    select_related entry — documented on
    ///    [`crate::relation::select_related::stitch_prefetches_into_joined`].
    ///
    /// # Short-circuit contract
    ///
    /// Honours the same `is_empty` short-circuit as every other
    /// terminal — a structural-none queryset returns `Ok(Vec::new())`
    /// without touching the database. An empty main result also
    /// short-circuits the prefetch pass — no prefetch loader runs
    /// when there are no parent rows to stitch against.
    ///
    /// # Executor shape
    ///
    /// Takes `&PgPool` concretely rather than the `impl Executor`
    /// generic `fetch_all` accepts, matching
    /// [`fetch_all_prefetched`](Self::fetch_all_prefetched): the
    /// prefetch loaders (when registered) run *after* the main query
    /// and need their own connection-from-pool without threading
    /// lifetimes through the type-erased loader signature.
    pub fn fetch_all_joined(
        self,
        pool: &sqlx::PgPool,
    ) -> impl Future<Output = Result<Vec<JoinedRow<T>>, DjogiError>> + Send + '_
    where
        T: FromJoinedRow,
        T::Pk: Clone + Send + Sync + 'static,
    {
        async move {
            // TASK6:empty_contract — structural-none queryset returns
            // `Vec::new()` without touching the DB. Mirrors every other
            // terminal.
            if self.is_empty() {
                return Ok(Vec::new());
            }

            // Snapshot prefetch + select_related paths before we build
            // the SQL — `build_select_joined` borrows the queryset
            // shape, not the path vectors.
            let select_related_paths = self.select_related_paths.clone();
            let prefetches = self.prefetch_paths.clone();

            // Build and execute the joined main query. `build_select_joined`
            // emits the aliased projection + LEFT JOINs when
            // `select_related_paths` is non-empty; with an empty path
            // list it degenerates to `SELECT {parent}.* FROM ...` — same
            // shape as `build_select` minus the `*` shortcut. The
            // decoded rows come back as raw `PgRow`s so both parent and
            // (per registered path) child can be extracted via
            // `FromJoinedRow::from_prefixed_row`.
            let mut qb = build_select_joined(&self);
            let rows: Vec<sqlx::postgres::PgRow> =
                qb.build().fetch_all(pool).await.map_err(DjogiError::from)?;

            // Decode each row into a JoinedRow<T> carrying any joined
            // children. `apply_select_related` is pure CPU work — no
            // additional SQL round trips.
            let joined = apply_select_related::<T>(rows, &select_related_paths)?;

            // If prefetches were also registered, fan them out and
            // stitch the resolved targets into the same JoinedRow<T>
            // values. `stitch_prefetches_into_joined` short-circuits
            // when `prefetches` is empty, so there's no cost for the
            // common `select_related`-only path.
            stitch_prefetches_into_joined(joined, &prefetches, pool).await
        }
    }

    /// Execute with `LIMIT 1` and return the first matching row or `None`.
    ///
    /// Unlike `fetch_one`, this does not care whether other rows exist — it
    /// is the terminal you reach for when you want "any row that matches"
    /// rather than "the unique row that matches". Pair it with
    /// [`QuerySet::order_by`] for a deterministic choice.
    pub fn first<'a, E>(
        self,
        executor: E,
    ) -> impl Future<Output = Result<Option<T>, DjogiError>> + Send
    where
        E: sqlx::Executor<'a, Database = sqlx::Postgres> + Send,
    {
        async move {
            // TASK6:empty_contract — structural-none returns `None` without
            // a round trip.
            if self.is_empty() {
                return Ok(None);
            }
            let mut qs = self;
            qs.limit = Some(1);
            let mut qb = build_select(&qs);
            let opt: Option<T> = qb
                .build_query_as::<T>()
                .fetch_optional(executor)
                .await
                .map_err(DjogiError::from)?;
            Ok(opt)
        }
    }
}

// ── Scalar terminals (no FromRow bound needed) ────────────────────────────

impl<T: Model> QuerySet<T> {
    /// `SELECT COUNT(*) FROM <table> [WHERE ...]`.
    ///
    /// Returns `i64` to match Postgres' `BIGINT` result of `COUNT(*)` and to
    /// leave headroom for tables that grow past `i32::MAX` rows. User code
    /// that needs a `usize` converts at the call site.
    pub fn count<'a, E>(self, executor: E) -> impl Future<Output = Result<i64, DjogiError>> + Send
    where
        E: sqlx::Executor<'a, Database = sqlx::Postgres> + Send,
    {
        async move {
            // TASK6:empty_contract — structural-none returns 0 without SQL.
            if self.is_empty() {
                return Ok(0);
            }
            let mut qb = build_count(&self);
            let n: i64 = qb
                .build_query_scalar::<i64>()
                .fetch_one(executor)
                .await
                .map_err(DjogiError::from)?;
            Ok(n)
        }
    }

    /// `SELECT EXISTS(SELECT 1 FROM <table> [WHERE ...] LIMIT 1)`.
    ///
    /// The `LIMIT 1` is inside the EXISTS subquery (see
    /// [`crate::query::sql::build_exists`]) so Postgres stops scanning at
    /// the first match — meaningful for large tables where even a count
    /// probe would touch many pages.
    pub fn exists<'a, E>(self, executor: E) -> impl Future<Output = Result<bool, DjogiError>> + Send
    where
        E: sqlx::Executor<'a, Database = sqlx::Postgres> + Send,
    {
        async move {
            // TASK6:empty_contract — structural-none returns false without SQL.
            if self.is_empty() {
                return Ok(false);
            }
            let mut qb = build_exists(&self);
            let b: bool = qb
                .build_query_scalar::<bool>()
                .fetch_one(executor)
                .await
                .map_err(DjogiError::from)?;
            Ok(b)
        }
    }
}
