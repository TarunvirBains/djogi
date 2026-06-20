//! Terminal read methods on [`QuerySet<T>`].
//! # What
//! Every method here is a terminal — it consumes the queryset, executes SQL
//! against a caller-provided `&mut DjogiContext`, and returns a decoded result
//! (`Vec<T>`, `T`, `Option<T>`, `i64`, `bool`). This is the **only** place
//! in the query layer that talks to the database.
//! # Why kept in its own file
//! Splitting read terminals out of `queryset.rs` keeps each file auditable:
//! the builder file deals only with structural transforms (filter/order/limit
//! accumulation), this file owns SQL execution + error mapping. Writes
//! (`update` / `delete`) land in a sibling `update.rs` module in a later
//! task rather than being mixed into this file. Each layer can be reviewed
//! for its own invariants (no SQL in queryset.rs; no mutation of accumulated
//! state in terminal.rs beyond the documented `limit` override in
//! `fetch_one` / `first`).
//! # Context dispatch
//! Each terminal takes `&mut DjogiContext` and calls through the context's
//! execution helpers (`query_all`, `query_opt`, `query_one`, `execute`),
//! which dispatch to either a pool-acquired connection or the active
//! transaction connection transparently. The same call site works against
//! a pool-backed context or a transaction-backed one.
//! # `is_empty` short-circuit contract
//! Every terminal honours the `QuerySet::none()` contract — a queryset
//! marked `is_empty = true` returns the empty result **without issuing any
//! SQL**:
//! | Method | Empty result |
//! |--------------|------------------------------------------------|
//! | `fetch_all` | `Ok(vec![])` |
//! | `fetch_one` | `Err(DjogiError::NotFound { table: T::... })` |
//! | `first` | `Ok(None)` |
//! | `count` | `Ok(0)` |
//! | `exists` | `Ok(false)` |
//! The grep marker `TASK6:empty_contract` on the `is_empty` field in
//! `queryset.rs` is the anchor for this contract — if the field shape ever
//! changes, that marker surfaces every terminal that needs updating.
//! # `fetch_one` row-count strategy
//! `fetch_one` expects **exactly one** row. Rather than issuing two round
//! trips (`COUNT(*)` then `SELECT`), it rewrites the user's `limit` to 2
//! before building the SELECT. A single-element result is success; a
//! two-element result proves "more than one matches" without scanning the
//! whole table, which matters for unbounded-cardinality filters (e.g.
//! `published = true` on a posts table). Zero rows -> `NotFound`.
//! # Why RPITIT (not `async fn`)
//! Every terminal returns `impl Future<Output = ...> + Send` rather than
//! using bare `async fn`. The explicit `+ Send` bound matches the
//! `Model` trait shape (`model.rs`) and guarantees the returned future can
//! be `.await`ed across task boundaries — required for any async runtime
//! context that spawns terminals onto a multi-thread runtime (e.g. an Axum
//! handler running on Tokio's multi-thread runtime under the opt-in `axum`
//! feature).
//! `clippy::manual_async_fn` fires on this pattern; the lint is allowed at
//! the module level because the explicit-bound form is the deliberate
//! choice, not an oversight.
//! [`ContextInner`]: crate::context::DjogiContext
#![allow(clippy::manual_async_fn)]

use crate::DjogiError;
use crate::context::DjogiContext;
use crate::model::Model;
use crate::pg::accumulator::{SqlAccumulator, as_params};
use crate::pg::decode::{FromJoinedPgRow, FromPgRow, try_get_scalar};
use crate::query::queryset::QuerySet;
use crate::query::sql::{build_count, build_exists, build_select, build_select_joined};
use crate::query::stream::{DEFAULT_FETCH_SIZE, ModelCursorStream, build_model_stream};
use crate::relation::joined_row::JoinedRow;
use crate::relation::prefetch::{PrefetchedRow, apply_prefetches};
use crate::relation::select_related::{apply_select_related, stitch_prefetches_into_joined};
use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;

// ── Auto-tenant helper ────────────────────────────────────────────────────

/// If `T` has a `tenant_key` and `ctx.auth()` carries a `tenant_id`, call
/// `ctx.ensure_tenant_set(tenant_id)` so the `app.tenant_id` GUC is active
/// before any SQL is issued.
/// This helper is shared by all QuerySet terminals. The borrow is structured
/// to avoid overlapping borrows: `ctx.auth()` borrows `ctx` immutably;
/// cloning the `String` drops that borrow before `ctx.ensure_tenant_set`
/// takes `&mut ctx`.
/// Behavior by case:
/// - `T::descriptor().tenant_key` is `None` → no-op (model has no RLS column).
/// - `ctx.auth()` is `None` → no-op (no auth context attached; pre-auth
///   or non-tenant code path).
/// - `ctx.auth().tenant_id` is `Some(tid)` → delegates to
///   `ctx.ensure_tenant_set(tid)` which re-issues `SET LOCAL` when the
///   requested tid differs from the currently-applied one.
/// - `ctx.auth().tenant_id` is `None` AND a previous tid is still applied
///   → clears the `app.tenant_id` GUC via `ctx.clear_tenant()`. Without
///   this reset, a `set_auth(auth_with_tenant)` followed by
///   `set_auth(auth_without_tenant)` inside one transaction would leave
///   the earlier tenant's `SET LOCAL` active silently — a cross-tenant
///   leak. A `tracing::warn!` is also emitted (unless
///   `ctx.with_no_tenant_scope()` was called) so the caller sees the
///   transition.
pub(crate) async fn auto_set_tenant<T: Model>(ctx: &mut DjogiContext) -> Result<(), DjogiError> {
    if T::descriptor().tenant_key.is_none() {
        return Ok(());
    }
    // No auth attached → hands-off. Users driving `set_tenant` manually
    // (explicit tenancy flow, admin tooling, test harnesses) must
    // not have their explicit GUC state clobbered by the auto-wiring.
    if ctx.auth().is_none() {
        return Ok(());
    }
    let tid: Option<String> = ctx.auth().and_then(|a| a.tenant_id.clone());
    match tid {
        Some(tid) => ctx.ensure_tenant_set(&tid).await?,
        None => {
            // Auth IS attached but carries no tenant_id. If an earlier
            // auth-driven ensure_tenant_set applied a tid, reset it so
            // subsequent queries don't run under the stale GUC — this is
            // the phase-boundary fix for the
            // "set_auth(auth_with_tenant) → set_auth(auth_without_tenant)
            // leaves old SET LOCAL active" bug class.
            if ctx.applied_tenant_id().is_some() {
                ctx.clear_tenant().await?;
            }
            if !ctx.tenant_scope_suppressed {
                tracing::warn!(
                    model = std::any::type_name::<T>(),
                    "auth attached but tenant_id is None on a tenant-keyed model; \
                     queries will span tenants — call ctx.with_no_tenant_scope() to suppress",
                );
            }
        }
    }
    Ok(())
}

// ── Row-returning terminals (require `T: FromPgRow`) ────────────────────────

impl<T: Model> QuerySet<T>
where
    T: FromPgRow,
{
    /// Build the `SELECT` SQL this queryset would execute, without
    /// touching a database. **Internal-test plumbing — never call this
    /// from adopter code.**
    /// Tests that pin SQL shape (column ordering, bind ordinals,
    /// portable predicate lowering, joined-select qualification) reach
    /// for this hook to assert the textual contract directly without
    /// needing `pg_stat_statements` server-side configuration. Mirrors
    /// the pre-existing `VisageQuerySet::__sql_for_test` shape.
    /// The double-underscore prefix is the framework's
    /// `feedback_macro_path_routing` convention for "macro-internal
    /// public-by-necessity surface, do not depend on" — same as
    /// `__make_field_ref_with_path` and friends. The method stays
    /// `pub` (cross-crate test access requires it) but `#[doc(hidden)]`
    /// keeps it out of rustdoc.
    /// Introduces this helper so the regression test
    /// `phase8eta_queryset_filter_preservation` can verify SQL parity
    /// through the post-flip root accessor surface. Returns the raw SQL
    /// string only; callers needing both SQL and binds use the
    /// underlying `build_select` (crate-private) instead.
    #[doc(hidden)]
    pub fn __sql_for_test(&self) -> Result<String, crate::DjogiError> {
        let acc = crate::query::sql::build_select(self).map_err(crate::DjogiError::from)?;
        let (sql, _binds) = acc.into_parts();
        Ok(sql)
    }
}

impl<T: Model> QuerySet<T>
where
    T: FromPgRow + Send + Unpin,
{
    /// Execute the query and collect every matching row into a `Vec<T>`.
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
            auto_set_tenant::<T>(ctx).await?;
            // Snapshot the cache binding before we consume `self` in
            // `build_select(&self)` — the SQL emitter borrows the
            // queryset, but the post-fetch hook needs to outlive that
            // borrow. 3. The clone is `Arc::clone` on
            // the trait-object handle (see `queryset.rs` Clone impl
            // for the cache_target field rationale), so it's cheap.
            let cache_target = self.cache_target.clone();
            let acc = build_select(&self).map_err(DjogiError::from)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let rows = ctx.query_all(&sql, &params).await?;
            let result: Vec<T> = rows
                .iter()
                .map(|r| T::from_pg_row(r))
                .collect::<Result<Vec<T>, _>>()?;
            // 3 post-fetch cache hook. Fires only when
            // `.cache(&punnu)` was called on the queryset — adopters
            // who didn't bind a Punnu pay zero (the `Option::is_none`
            // branch is the optimiser's hot path here). The trait
            // object's `insert(&T)` clones internally where the
            // `T: Clone` bound is satisfied (the `.cache(...)`
            // builder requires it), so this terminal stays free of
            // a `T: Clone` bound and the existing fetch_all surface
            // is unchanged for callers who don't bind a cache.
            // Errors from `Punnu::insert` are logged-and-swallowed
            // inside `PunnuCacheTarget::insert`.
            if let Some(target) = cache_target.as_ref() {
                for row in &result {
                    target.insert(row).await;
                }
            }
            Ok(result)
        }
    }

    /// Execute the query and require **exactly one** matching row.
    /// - Zero rows -> [`DjogiError::NotFound`].
    /// - Two or more rows -> [`DjogiError::MultipleObjects`] (via `LIMIT 2`;
    ///   `count_seen = 2`).
    ///   User-supplied `limit` on the queryset is ignored — this terminal
    ///   owns the row-count probe.
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
            auto_set_tenant::<T>(ctx).await?;
            // Override the user's LIMIT (if any) with 2 so we can
            // distinguish the single-row success path from the
            // multiple-rows error path without a `COUNT(*)` round trip.
            let mut qs = self;
            qs.limit = Some(2);
            // 3: snapshot the cache binding before we
            // consume `qs` in `build_select(&qs)`. The success path
            // below feeds the (sole) decoded row to the bound Punnu;
            // the error paths (NotFound / MultipleObjects) skip the
            // hook because no user-facing row materialised — the
            // caller wouldn't have been able to observe a cached
            // entry from a failed lookup either, and inserting a row
            // that the caller never saw would surface in the Punnu's
            // event stream as an unexplained Insert.
            let cache_target = qs.cache_target.clone();
            let acc = build_select(&qs).map_err(DjogiError::from)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let rows = ctx.query_all(&sql, &params).await?;
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
                    let decoded = T::from_pg_row(&row)?;
                    // Post-fetch cache hook (success path only). See
                    // the comment on `cache_target` snapshot above for
                    // why the error branches skip the insert.
                    if let Some(target) = cache_target.as_ref() {
                        target.insert(&decoded).await;
                    }
                    Ok(decoded)
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
    /// # Why a separate terminal (not a change to `fetch_all`)
    /// Preserving [`fetch_all`](Self::fetch_all)'s `Vec<T>` return type
    /// keeps the terminal stable across : a queryset
    /// built without prefetches and fetched via `fetch_all` returns
    /// exactly what it did before Task 4 landed. Prefetches are an
    /// opt-in extension reachable through the dedicated
    /// `fetch_all_prefetched` entry — no pre-existing call site is
    /// forced into `Vec<PrefetchedRow<T>>`. This also makes prefetch
    /// registrations free on querysets whose terminal happens to be
    /// `fetch_all`: the `prefetch_paths` field is ignored on that path,
    /// which is documented on the field itself.
    /// # Short-circuit contract
    /// Honours the same `is_empty` short-circuit as the other terminals:
    /// a structural-none queryset returns `Ok(Vec::new())` without
    /// touching the database. An empty main result also short-circuits
    /// the prefetch pass — no prefetch loader runs when there are no
    /// parent rows to stitch against.
    /// # Context shape
    /// Takes `&mut DjogiContext` matching every other terminal. Both
    /// pool-backed and transaction-backed contexts are supported: the
    /// prefetch loader threads `&mut ContextInner` internally and
    /// dispatches each fetch to either the pool or the outer
    /// transaction via inline-match. Prefetch fan-out inside an
    /// `atomic` scope works transparently and sees
    /// the scope's uncommitted writes.
    // TODO: does NOT yet honour `cache_target` — see the
    // remote anchor at the bottom of this impl block (above `stream`)
    // for the deferral rationale.
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
            auto_set_tenant::<T>(ctx).await?;

            // Snapshot prefetch paths before we consume `self` in the
            // main-query build. The shared SQL emitter borrows the
            // queryset, so we pull out what we need first.
            let prefetches = self.prefetch_paths.clone();

            // Main query — identical shape to `fetch_all`.
            let acc = build_select(&self).map_err(DjogiError::from)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let pg_rows = ctx.query_all(&sql, &params).await?;
            let rows: Vec<T> = pg_rows
                .iter()
                .map(|r| T::from_pg_row(r))
                .collect::<Result<Vec<T>, _>>()?;

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
    /// # Why a separate terminal (not a change to `fetch_all`)
    /// Preserving [`fetch_all`](Self::fetch_all)'s `Vec<T>` return
    /// type keeps the terminal stable across : a
    /// queryset built without select_related and fetched via
    /// `fetch_all` returns exactly what it did before Task 5 landed.
    /// select_related is an opt-in extension reachable through the
    /// dedicated `fetch_all_joined` entry — no pre-existing call site
    /// is forced into `Vec<JoinedRow<T>>`. Registrations on the
    /// `select_related_paths` queryset field are ignored on the plain
    /// `fetch_all` path, matching the free-register-on-any-terminal
    /// behaviour `prefetch` already documents.
    /// # Composes with `.prefetch(...)`
    /// When the queryset carries both select_related and prefetch
    /// registrations, the terminal runs:
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
    /// # Short-circuit contract
    /// Honours the same `is_empty` short-circuit as every other
    /// terminal — a structural-none queryset returns `Ok(Vec::new())`
    /// without touching the database. An empty main result also
    /// short-circuits the prefetch pass — no prefetch loader runs
    /// when there are no parent rows to stitch against.
    /// # Context shape
    /// Takes `&mut DjogiContext`, matching
    /// [`fetch_all_prefetched`](Self::fetch_all_prefetched). Pool-backed
    /// and transaction-backed contexts are both supported — the main
    /// query and prefetch fan-out both dispatch through the context
    /// helpers, so `select_related` works inside an
    /// `atomic()` scope and sees the scope's uncommitted writes.
    // TODO: does NOT yet honour `cache_target` — see the
    // remote anchor at the bottom of this impl block (above `stream`)
    // for the deferral rationale.
    pub fn fetch_all_joined<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Vec<JoinedRow<T>>, DjogiError>> + Send + 'ctx
    where
        T: FromJoinedPgRow + 'ctx,
        T::Pk: Clone + Send + Sync + 'static,
    {
        async move {
            // TASK6:empty_contract — structural-none queryset returns
            // `Vec::new()` without touching the DB. Mirrors every other
            // terminal.
            if self.is_empty() {
                return Ok(Vec::new());
            }
            auto_set_tenant::<T>(ctx).await?;

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
            // decoded rows come back as raw `tokio_postgres::Row`s so
            // both parent and (per registered path) child can be
            // extracted via `FromJoinedPgRow::from_joined_pg_row`.
            let acc = build_select_joined(&self).map_err(DjogiError::from)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let rows: Vec<tokio_postgres::Row> = ctx.query_all(&sql, &params).await?;

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
            auto_set_tenant::<T>(ctx).await?;
            let mut qs = self;
            qs.limit = Some(1);
            // 3: snapshot before the SQL emitter
            // borrows `qs`.
            let cache_target = qs.cache_target.clone();
            let acc = build_select(&qs).map_err(DjogiError::from)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let opt = ctx.query_opt(&sql, &params).await?;
            let decoded = opt.as_ref().map(|r| T::from_pg_row(r)).transpose()?;
            // 3 post-fetch cache hook — fires only on
            // the `Some(_)` branch (no row, no insert). Error
            // semantics + bound rationale match the `fetch_all`
            // hook above.
            if let (Some(target), Some(row)) = (cache_target.as_ref(), decoded.as_ref()) {
                target.insert(row).await;
            }
            Ok(decoded)
        }
    }

    /// Find-or-create: return the first row matching the queryset, or
    /// create a new row from `factory()` when none matches.
    /// The tuple's second element reports which branch ran
    /// `true` when a new row was inserted, `false` when an existing row
    /// was found. This mirrors the Django ORM's `get_or_create` shape
    /// so app-level consumers have the same "insert-if-absent" idiom
    /// they're used to.
    /// # Lookup semantics
    /// The lookup uses [`first`](Self::first) — the same `LIMIT 1`
    /// probe every "any row that matches" caller uses. If the
    /// queryset's filter is not unique, this method returns the first
    /// row the database chooses (non-deterministic without an
    /// `order_by`). Call sites that need exactly-one semantics should
    /// reach for `fetch_one` followed by a separate `create` branch
    /// instead.
    /// # Transactions and races
    /// The SELECT and the INSERT are two separate statements. Under
    /// concurrent writers a second caller may slip between the
    /// probe and the create — the INSERT then collides with whatever
    /// uniqueness constraint covers the filter. Wrap the call in
    /// [`atomic`](crate::transaction::atomic) + one of:
    /// - `select_for_update()` on the queryset to serialise lookups
    /// - an `ON CONFLICT` clause on the underlying table
    ///   when the caller needs strict once-only semantics.
    ///   Task 7.5 adds `create_or_find` for the conflict-key path.
    /// # Short-circuit
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
    /// The tuple's second element reports which branch ran
    /// `true` when a new row was inserted, `false` when an existing
    /// row was updated in place.
    /// # Semantics
    /// - Found branch: `updater(&mut row)` runs, then
    ///   [`save`](crate::model::Model::save) rehydrates the row from
    ///   `UPDATE ... RETURNING *` — `updated_at` advances and any
    ///   trigger-mutated column surfaces in the returned `T`.
    /// - Missing branch: `factory()` runs and
    ///   [`create`](crate::model::Model::create) inserts the new row;
    ///   the returned `T` is the `RETURNING *` rehydration.
    ///   `updater` takes `&mut T` so callers can mutate multiple fields
    ///   in one pass without needing to rebuild the struct.
    /// # Race caveat
    /// Same non-atomic caveat as [`get_or_create`](Self::get_or_create)
    /// the SELECT and the UPDATE/INSERT are distinct statements.
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
    /// One round trip. The generated SQL is
    /// `SELECT * FROM <table> WHERE id IN ($1, $2, ...)` — one bound
    /// parameter per id. Postgres' bind-parameter cap is 65_535; larger
    /// id batches should be chunked by the caller.
    /// # Why on `QuerySet`, not `Model`
    /// The queryset receiver means callers can still stack filters and
    /// orderings before the PK probe:
    /// ```rust,ignore
    /// Account::objects()
    ///     .filter(|f| f.tenant_id.eq(tenant))
    ///     .in_bulk(&mut ctx, ids)
    ///     .await?;
    /// ```
    /// A bare `Account::in_bulk(ctx, ids)` is still reachable as
    /// `Account::objects().in_bulk(ctx, ids)`.
    /// # Empty input
    /// `ids.is_empty()` returns `Ok(HashMap::new())` without a round
    /// trip — `id IN ()` is invalid SQL, and an empty probe always
    /// yields an empty map anyway.
    /// # Short-circuit
    /// Honours the `is_empty` structural-none contract — a
    /// `QuerySet::none()`-derived queryset returns `Ok(HashMap::new())`
    /// without SQL emission, matching every other terminal.
    // TODO: does NOT yet honour `cache_target` — see the
    // remote anchor at the bottom of this impl block (above `stream`)
    // for the deferral rationale.
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
            auto_set_tenant::<T>(ctx).await?;
            // SELECT by PK list, AND-composed with `self.condition`.
            //
            // The PK `IN (...)` predicate is still hand-built rather than
            // routed through `build_select`: a generic `QuerySet<T>` has no
            // handle on the `{Model}Fields::id` FieldRef (the field bag is a
            // per-model ZST emitted by the macro), and `T::Pk` has no generic
            // `FilterValue` conversion — the typed `FieldRef<M, V>` API is the
            // only producer of `FilterValue::List`. So the IN list binds
            // through `push_list_binds` (which only needs `T::Pk: ToSql`).
            //
            // But the queryset's accumulated `self.condition` (which now
            // carries the soft-delete default filter seeded by `QuerySet::new`,
            // plus any caller `.filter(...)`) IS composed: when non-vacuous it
            // is emitted by the shared condition emitter and AND-ed with the PK
            // IN clause, so `objects().in_bulk(...)` on a soft-deletable model
            // does not leak deleted rows. Parameter numbering stays positional
            // because the condition binds are pushed before the IN binds and
            // the accumulator increments `$n` in push order.
            let mut acc = SqlAccumulator::new("SELECT ");
            acc.push_sql(<T as FromPgRow>::COLUMN_LIST);
            acc.push_sql(" FROM ");
            acc.push_sql(T::table_name());
            acc.push_sql(" WHERE ");
            let has_condition = crate::query::sql::has_consumer_where(&self);
            if has_condition {
                acc.push_sql("(");
                crate::query::sql::push_consumer_predicate_only(&mut acc, &self)
                    .map_err(DjogiError::from)?;
                acc.push_sql(") AND ");
            }
            acc.push_sql("id IN (");
            acc.push_list_binds(ids.iter().cloned());
            acc.push_sql(")");
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let pg_rows = ctx.query_all(&sql, &params).await?;
            let mut out: HashMap<T::Pk, T> = HashMap::with_capacity(pg_rows.len());
            for row in &pg_rows {
                let item = T::from_pg_row(row)?;
                out.insert(item.pk_value().clone(), item);
            }
            Ok(out)
        }
    }
}

// ── Scalar terminals (no FromPgRow bound needed) ─────────────────────────────

impl<T: Model> QuerySet<T> {
    /// `SELECT COUNT(*) FROM <table> [WHERE ...]`.
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
            auto_set_tenant::<T>(ctx).await?;
            let acc = build_count(&self).map_err(DjogiError::from)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let row = ctx.query_one(&sql, &params).await?;
            let n: i64 = try_get_scalar(&row, 0)?;
            Ok(n)
        }
    }

    /// `SELECT EXISTS(SELECT 1 FROM <table> [WHERE ...] LIMIT 1)`.
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
            auto_set_tenant::<T>(ctx).await?;
            let acc = build_exists(&self).map_err(DjogiError::from)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let row = ctx.query_one(&sql, &params).await?;
            let b: bool = try_get_scalar(&row, 0)?;
            Ok(b)
        }
    }
}

// ── Streaming terminal (requires `T: FromPgRow + Unpin + Send`) ─────────────

impl<T: Model> QuerySet<T>
where
    T: FromPgRow + Unpin + Send,
{
    /// Stream rows from the database using a Postgres named cursor.
    /// Returns `impl Stream<Item = Result<T, DjogiError>>` where rows are
    /// decoded one at a time as they arrive. The stream back-pressures
    /// naturally — no rows are held beyond one `fetch_size` batch.
    /// # Transaction requirement
    /// Named Postgres cursors are transaction-local. This terminal requires
    /// `ctx` to be backed by an active transaction (i.e. called inside an
    /// [`atomic()`](crate::transaction::atomic) scope). Calling `stream` on a
    /// pool-backed context returns
    /// `Err(DjogiError::StreamOutsideTransaction)` immediately — the error
    /// surfaces at construction time, not on the first `poll_next`.
    /// # Fetch size
    /// Default is `1000` rows per `FETCH` round trip. Use
    /// [`stream_with_fetch_size`](Self::stream_with_fetch_size) to override.
    /// # Example
    /// ```ignore
    /// use futures::StreamExt;
    ///
    /// atomic(&mut ctx, |ctx| Box::pin(async move {
    ///     let mut stream = Post::objects()
    ///         .filter(|f| f.published.eq(true))
    ///         .stream(ctx)
    ///         .await?;
    ///
    ///     while let Some(result) = stream.next().await {
    ///         let post = result?;
    ///         // process post …
    ///     }
    ///     Ok(())
    /// })).await?;
    /// ```
    // TODO: stream surfaces (`stream`,
    // `stream_with_fetch_size`) and the joined / prefetched
    // terminals (`fetch_all_prefetched`, `fetch_all_joined`,
    // `in_bulk`) intentionally do NOT yet honour `cache_target`.
    // The post-fetch hook is scoped to the three terminals
    // `fetch_all`, `first`, `fetch_one`. The streaming surface
    // needs a separate design pass (back-pressure interactions
    // with `Punnu::insert`'s async write-through, and the
    // semantics of "cache mid-stream" if the consumer aborts
    // before exhausting the cursor) that's deliberately a future
    // commit's concern.
    pub async fn stream<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> Result<ModelCursorStream<'ctx, T>, DjogiError>
    where
        T: 'ctx,
    {
        self.stream_with_fetch_size(ctx, DEFAULT_FETCH_SIZE).await
    }

    /// Stream rows using a Postgres named cursor with a custom fetch size.
    /// Like [`stream`](Self::stream) but lets the caller override the number
    /// of rows retrieved per `FETCH` round trip. Smaller values (e.g. `100`)
    /// reduce per-batch memory pressure; larger values (e.g. `10_000`) reduce
    /// round-trip overhead for high-throughput exports.
    /// `fetch_size` must be at least `1`. Passing `0` returns
    /// `Err(DjogiError::Validation)` immediately.
    /// # Transaction requirement
    /// Same as [`stream`](Self::stream) — `ctx` must be transaction-backed.
    pub async fn stream_with_fetch_size<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
        fetch_size: u32,
    ) -> Result<ModelCursorStream<'ctx, T>, DjogiError>
    where
        T: 'ctx,
    {
        if fetch_size == 0 {
            return Err(DjogiError::Validation(
                "stream fetch_size must be at least 1".to_owned(),
            ));
        }
        auto_set_tenant::<T>(ctx).await?;

        // Short-circuit on structural-none: an empty QuerySet has no rows to
        // stream. The stream would immediately close the cursor after a zero-
        // row DECLARE+FETCH; skip the round trip by returning a no-op stream.
        // Note: we cannot early-return an `impl Stream` here because Rust
        // requires a concrete return type. We build a real cursor stream but
        // let the standard cursor exhaustion path terminate it after one
        // FETCH that returns 0 rows. This is simpler and more correct than
        // a special empty-stream type.
        let acc = build_select(&self).map_err(DjogiError::from)?;
        let (sql, binds) = acc.into_parts();
        let params = as_params(&binds);
        build_model_stream(ctx, &sql, &params, fetch_size).await
    }
}
