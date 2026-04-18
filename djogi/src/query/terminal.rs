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
use crate::query::sql::{build_count, build_exists, build_select};
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
                n => Err(DjogiError::multiple_objects(T::table_name(), n)),
            }
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
