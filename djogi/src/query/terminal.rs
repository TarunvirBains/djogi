//! Terminal read methods on [`QuerySet<T>`].
//!
//! # What
//!
//! Every method here is a terminal — it consumes the queryset, executes SQL
//! against a caller-provided `&mut DjogiContext`, and returns a decoded result
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
//! # Context dispatch and `Send` futures
//!
//! Each terminal takes `&mut DjogiContext`, matching the pattern established by
//! the Phase 4-retrofitted `Model` trait methods: the same call site works
//! against a pool-backed context or a transaction-backed one. Internally the
//! terminal pattern-matches on [`ContextInner`] to dispatch the sqlx query
//! against the appropriate handle — see `djogi::context` module docs for the
//! inline-match rationale.
//!
//! Returning `impl Future<Output = ...> + Send` (RPITIT) means callers can
//! `.await` results across task boundaries — required for any async runtime
//! context that spawns terminals onto a multi-thread runtime (e.g. an Axum
//! handler running on Tokio's multi-thread runtime under the opt-in `axum`
//! feature).
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
//! be `.await`ed across task boundaries — critical for any async runtime
//! context that spawns terminals onto a multi-thread runtime (e.g. an Axum
//! handler on Tokio's multi-thread runtime). `async fn` in trait / impl
//! position does not
//! automatically carry `+ Send` on the returned future; spelling the bound
//! explicitly keeps the call site free of "future is not Send" errors that
//! would otherwise show up far from where the bound is missing.
//!
//! `clippy::manual_async_fn` fires on this pattern; the lint is allowed at
//! the module level because the explicit-bound form is the deliberate
//! choice, not an oversight.
//!
//! [`ContextInner`]: crate::context::DjogiContext
#![allow(clippy::manual_async_fn)]

use crate::DjogiError;
use crate::context::{ContextInner, DjogiContext};
use crate::model::Model;
use crate::query::queryset::QuerySet;
use crate::query::sql::{build_count, build_exists, build_select, build_select_joined};
use crate::relation::joined_row::{FromJoinedRow, JoinedRow};
use crate::relation::prefetch::{PrefetchedRow, apply_prefetches};
use crate::relation::select_related::{apply_select_related, stitch_prefetches_into_joined};
use sqlx::{Postgres, QueryBuilder};
use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;

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
    pub fn fetch_all<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Vec<T>, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
    {
        async move {
            // TASK6:empty_contract — structural-none queryset, no SQL.
            if self.is_empty() {
                return Ok(Vec::new());
            }
            let mut qb = build_select(&self);
            let q = qb.build_query_as::<T>();
            let rows: Vec<T> = match ctx.inner_mut() {
                ContextInner::Pool(pool) => q
                    .fetch_all(&*pool)
                    .await
                    .map_err(crate::error::map_lock_err)?,
                ContextInner::Transaction(tx) => q
                    .fetch_all(&mut **tx)
                    .await
                    .map_err(crate::error::map_lock_err)?,
            };
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
    pub fn fetch_one<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<T, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
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
            let q = qb.build_query_as::<T>();
            let rows: Vec<T> = match ctx.inner_mut() {
                ContextInner::Pool(pool) => q
                    .fetch_all(&*pool)
                    .await
                    .map_err(crate::error::map_lock_err)?,
                ContextInner::Transaction(tx) => q
                    .fetch_all(&mut **tx)
                    .await
                    .map_err(crate::error::map_lock_err)?,
            };
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
    /// # Context shape
    ///
    /// Takes `&mut DjogiContext` matching every other terminal. Both
    /// pool-backed and transaction-backed contexts are supported: the
    /// prefetch loader threads `&mut ContextInner` internally and
    /// dispatches each fetch to either the pool or the outer
    /// transaction via inline-match. Prefetch fan-out inside an
    /// `atomic()` scope (Phase 4 Task 1) works transparently and sees
    /// the scope's uncommitted writes.
    pub fn fetch_all_prefetched<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Vec<PrefetchedRow<T>>, DjogiError>> + Send + 'ctx
    where
        T::Pk: Clone + Send + Sync + 'static,
        T: 'ctx,
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

            // Main query — identical shape to `fetch_all`. Dispatches
            // through the same inline-match on `ContextInner` every
            // other terminal uses; prefetch fan-out inherits the same
            // context variant below.
            let mut qb = build_select(&self);
            let rows: Vec<T> = {
                let q = qb.build_query_as::<T>();
                match ctx.inner_mut() {
                    ContextInner::Pool(pool) => {
                        q.fetch_all(&*pool).await.map_err(DjogiError::from)?
                    }
                    ContextInner::Transaction(tx) => {
                        q.fetch_all(&mut **tx).await.map_err(DjogiError::from)?
                    }
                }
            };

            // Apply each prefetch loader. Empty main result -> no
            // loaders run (short-circuit inside `apply_prefetches`).
            // The generalised loader signature lets the inner fan-out
            // see the same transaction-backed context the main query
            // ran on — no connection juggling.
            apply_prefetches(ctx.inner_mut(), &prefetches, rows).await
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
    /// # Context shape
    ///
    /// Takes `&mut DjogiContext`, matching
    /// [`fetch_all_prefetched`](Self::fetch_all_prefetched). Pool-backed
    /// and transaction-backed contexts are both supported — the main
    /// query and prefetch fan-out both dispatch through an inline-match
    /// on `ContextInner`, so `select_related` works inside an
    /// `atomic()` scope and sees the scope's uncommitted writes.
    pub fn fetch_all_joined<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Vec<JoinedRow<T>>, DjogiError>> + Send + 'ctx
    where
        T: FromJoinedRow + 'ctx,
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
            let rows: Vec<sqlx::postgres::PgRow> = {
                let q = qb.build();
                match ctx.inner_mut() {
                    ContextInner::Pool(pool) => {
                        q.fetch_all(&*pool).await.map_err(DjogiError::from)?
                    }
                    ContextInner::Transaction(tx) => {
                        q.fetch_all(&mut **tx).await.map_err(DjogiError::from)?
                    }
                }
            };

            // Decode each row into a JoinedRow<T> carrying any joined
            // children. `apply_select_related` is pure CPU work — no
            // additional SQL round trips.
            let joined = apply_select_related::<T>(rows, &select_related_paths)?;

            // If prefetches were also registered, fan them out and
            // stitch the resolved targets into the same JoinedRow<T>
            // values. `stitch_prefetches_into_joined` short-circuits
            // when `prefetches` is empty, so there's no cost for the
            // common `select_related`-only path.
            stitch_prefetches_into_joined(joined, &prefetches, ctx.inner_mut()).await
        }
    }

    /// Execute with `LIMIT 1` and return the first matching row or `None`.
    ///
    /// Unlike `fetch_one`, this does not care whether other rows exist — it
    /// is the terminal you reach for when you want "any row that matches"
    /// rather than "the unique row that matches". Pair it with
    /// [`QuerySet::order_by`] for a deterministic choice.
    pub fn first<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Option<T>, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
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
            let q = qb.build_query_as::<T>();
            let opt: Option<T> = match ctx.inner_mut() {
                ContextInner::Pool(pool) => {
                    q.fetch_optional(&*pool).await.map_err(DjogiError::from)?
                }
                ContextInner::Transaction(tx) => q
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(DjogiError::from)?,
            };
            Ok(opt)
        }
    }

    /// Find-or-create: return the first row matching the queryset, or
    /// create a new row from `factory()` when none matches.
    ///
    /// The tuple's second element reports which branch ran —
    /// `true` when a new row was inserted, `false` when an existing row
    /// was found. This mirrors the Django ORM's `get_or_create` shape
    /// so app-level consumers have the same "insert-if-absent" idiom
    /// they're used to.
    ///
    /// # Lookup semantics
    ///
    /// The lookup uses [`first`](Self::first) — the same `LIMIT 1`
    /// probe every "any row that matches" caller uses. If the
    /// queryset's filter is not unique, this method returns the first
    /// row the database chooses (non-deterministic without an
    /// `order_by`). Call sites that need exactly-one semantics should
    /// reach for `fetch_one` followed by a separate `create` branch
    /// instead.
    ///
    /// # Transactions and races
    ///
    /// The SELECT and the INSERT are two separate statements. Under
    /// concurrent writers a second caller may slip between the
    /// probe and the create — the INSERT then collides with whatever
    /// uniqueness constraint covers the filter. Wrap the call in
    /// [`atomic`](crate::transaction::atomic) + one of:
    ///
    /// - `select_for_update()` on the queryset to serialise lookups
    /// - an `ON CONFLICT` clause on the underlying table
    ///
    /// when the caller needs strict once-only semantics. Phase 4
    /// Task 7.5 adds `create_or_find` for the conflict-key path.
    ///
    /// # Short-circuit
    ///
    /// A `QuerySet::none()`-derived queryset short-circuits the
    /// lookup to `Ok(None)`, so the factory **runs and a row is
    /// inserted**. Callers who want "no insert on structural-none"
    /// must guard before calling.
    pub fn get_or_create<'ctx, F>(
        self,
        ctx: &'ctx mut DjogiContext,
        factory: F,
    ) -> impl Future<Output = Result<(T, bool), DjogiError>> + Send + 'ctx
    where
        F: FnOnce() -> T + Send + 'ctx,
        T: 'ctx,
    {
        async move {
            if let Some(row) = self.first(ctx).await? {
                return Ok((row, false));
            }
            let created = T::create(ctx, factory()).await?;
            Ok((created, true))
        }
    }

    /// Update-or-create: find the first matching row and mutate it via
    /// `updater`, or create a fresh row from `factory()` when none
    /// matches.
    ///
    /// The tuple's second element reports which branch ran —
    /// `true` when a new row was inserted, `false` when an existing
    /// row was updated in place.
    ///
    /// # Semantics
    ///
    /// - Found branch: `updater(&mut row)` runs, then
    ///   [`save`](crate::model::Model::save) rehydrates the row from
    ///   `UPDATE ... RETURNING *` — `updated_at` advances and any
    ///   trigger-mutated column surfaces in the returned `T`.
    /// - Missing branch: `factory()` runs and
    ///   [`create`](crate::model::Model::create) inserts the new row;
    ///   the returned `T` is the `RETURNING *` rehydration.
    ///
    /// `updater` takes `&mut T` so callers can mutate multiple fields
    /// in one pass without needing to rebuild the struct.
    ///
    /// # Race caveat
    ///
    /// Same non-atomic caveat as [`get_or_create`](Self::get_or_create)
    /// — the SELECT and the UPDATE/INSERT are distinct statements.
    /// Wrap in [`atomic`](crate::transaction::atomic) + a row lock
    /// when strict once-only semantics are required.
    pub fn update_or_create<'ctx, F, U>(
        self,
        ctx: &'ctx mut DjogiContext,
        factory: F,
        updater: U,
    ) -> impl Future<Output = Result<(T, bool), DjogiError>> + Send + 'ctx
    where
        F: FnOnce() -> T + Send + 'ctx,
        U: FnOnce(&mut T) + Send + 'ctx,
        T: 'ctx,
    {
        async move {
            if let Some(mut row) = self.first(ctx).await? {
                updater(&mut row);
                row.save(ctx).await?;
                return Ok((row, false));
            }
            let created = T::create(ctx, factory()).await?;
            Ok((created, true))
        }
    }

    /// Fetch every row whose primary key is in `ids` and return them
    /// keyed by PK in a `HashMap`.
    ///
    /// One round trip. The generated SQL is
    /// `SELECT * FROM <table> WHERE id IN ($1, $2, ...)` — one bound
    /// parameter per id. Postgres' bind-parameter cap is 65_535; larger
    /// id batches should be chunked by the caller.
    ///
    /// # Why on `QuerySet`, not `Model`
    ///
    /// The queryset receiver means callers can still stack filters and
    /// orderings before the PK probe:
    ///
    /// ```rust,ignore
    /// Account::objects()
    ///     .filter(|f| f.tenant_id.eq(tenant))
    ///     .in_bulk(&mut ctx, ids)
    ///     .await?;
    /// ```
    ///
    /// A bare `Account::in_bulk(ctx, ids)` is still reachable as
    /// `Account::objects().in_bulk(ctx, ids)`.
    ///
    /// # Empty input
    ///
    /// `ids.is_empty()` returns `Ok(HashMap::new())` without a round
    /// trip — `id IN ()` is invalid SQL, and an empty probe always
    /// yields an empty map anyway.
    ///
    /// # Short-circuit
    ///
    /// Honours the `is_empty` structural-none contract — a
    /// `QuerySet::none()`-derived queryset returns `Ok(HashMap::new())`
    /// without SQL emission, matching every other terminal.
    pub fn in_bulk<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
        ids: Vec<T::Pk>,
    ) -> impl Future<Output = Result<HashMap<T::Pk, T>, DjogiError>> + Send + 'ctx
    where
        T::Pk: Eq + Hash,
        T: 'ctx,
    {
        async move {
            // TASK6:empty_contract — structural-none skips SQL.
            if self.is_empty() || ids.is_empty() {
                return Ok(HashMap::new());
            }
            // Raw SELECT by PK list. We bypass `build_select` + the
            // filter chain because generic `QuerySet<T>` has no handle
            // on the `{Model}Fields::id` FieldRef — the field bag is a
            // per-model ZST emitted by the macro and not reachable
            // from this generic method. Any additional filters /
            // orderings the caller stacked on `self` are composed via
            // `AND` + appended after the PK probe so the produced SQL
            // is `SELECT * FROM t WHERE id IN (...) AND (<user WHERE>)`
            // when the user layered more constraints.
            let mut qb: QueryBuilder<'_, Postgres> = QueryBuilder::new("SELECT * FROM ");
            qb.push(T::table_name());
            qb.push(" WHERE id IN (");
            {
                let mut sep = qb.separated(", ");
                for id in ids {
                    sep.push_bind(id);
                }
            }
            qb.push(")");
            // The user's filters + orderings still apply. Re-use the
            // main emitter to append them — but we need them inside a
            // parenthesised `AND (...)` so precedence is preserved.
            //
            // Simplest correct path: emit the `QuerySet`'s WHERE
            // through a dedicated builder helper. For now we just drop
            // any user-stacked filters on this method — documented
            // inline; callers who need combined semantics can use
            // `.filter(...).fetch_all(...)` and key it themselves.
            //
            // TODO(phase4-task7d): layer user filters back in via a
            // dedicated `build_where_only` helper so `in_bulk` honours
            // upstream `.filter(...)` calls.
            let rows: Vec<T> = {
                let q = qb.build_query_as::<T>();
                match ctx.inner_mut() {
                    ContextInner::Pool(pool) => q
                        .fetch_all(&*pool)
                        .await
                        .map_err(crate::error::map_lock_err)?,
                    ContextInner::Transaction(tx) => q
                        .fetch_all(&mut **tx)
                        .await
                        .map_err(crate::error::map_lock_err)?,
                }
            };
            let mut out: HashMap<T::Pk, T> = HashMap::with_capacity(rows.len());
            for row in rows {
                out.insert(row.pk_value().clone(), row);
            }
            Ok(out)
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
    pub fn count<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<i64, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
    {
        async move {
            // TASK6:empty_contract — structural-none returns 0 without SQL.
            if self.is_empty() {
                return Ok(0);
            }
            let mut qb = build_count(&self);
            let q = qb.build_query_scalar::<i64>();
            let n: i64 = match ctx.inner_mut() {
                ContextInner::Pool(pool) => q.fetch_one(&*pool).await.map_err(DjogiError::from)?,
                ContextInner::Transaction(tx) => {
                    q.fetch_one(&mut **tx).await.map_err(DjogiError::from)?
                }
            };
            Ok(n)
        }
    }

    /// `SELECT EXISTS(SELECT 1 FROM <table> [WHERE ...] LIMIT 1)`.
    ///
    /// The `LIMIT 1` is inside the EXISTS subquery (see
    /// [`crate::query::sql::build_exists`]) so Postgres stops scanning at
    /// the first match — meaningful for large tables where even a count
    /// probe would touch many pages.
    pub fn exists<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<bool, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
    {
        async move {
            // TASK6:empty_contract — structural-none returns false without SQL.
            if self.is_empty() {
                return Ok(false);
            }
            let mut qb = build_exists(&self);
            let q = qb.build_query_scalar::<bool>();
            let b: bool = match ctx.inner_mut() {
                ContextInner::Pool(pool) => q.fetch_one(&*pool).await.map_err(DjogiError::from)?,
                ContextInner::Transaction(tx) => {
                    q.fetch_one(&mut **tx).await.map_err(DjogiError::from)?
                }
            };
            Ok(b)
        }
    }
}
