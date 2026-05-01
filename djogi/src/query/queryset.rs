//! `QuerySet<T>` — the lazy query builder.
//!
//! # What
//!
//! [`QuerySet<T>`] accumulates filter conditions, ordering, distinct mode,
//! and pagination (`limit` / `offset`) without hitting the database. Every
//! builder method (`filter`, `exclude`, `order_by`, `limit`, `offset`,
//! `distinct`, `distinct_on`) consumes `self` and returns `Self`, so a
//! `QuerySet` is immutable-by-convention: composition never mutates an
//! existing queryset in place.
//!
//! `T::objects()` (default method on the `Model` trait, added in Task 5)
//! constructs an empty `QuerySet<T>` — no filters, no ordering, no limit.
//! This is the entry point for every query.
//!
//! # Why
//!
//! Terminal methods (Task 6 — `fetch_all`, `fetch_one`, `count`, `exists`,
//! `first`, `update`, `delete`) are the **only** place SQL is generated or
//! executed. Everything else is a cheap structural transformation: `Condition`
//! trees shared across clones, small `Vec`s for ordering and distinct_on
//! column lists, POD enums for distinct mode.
//!
//! Builder methods that append to accumulators (currently only `order_by`)
//! follow Django-style semantics: calling `.order_by(...)` twice **appends**
//! rather than replaces, so library code can add a stable tiebreaker without
//! clobbering the caller's primary ordering. Replace semantics would force
//! every caller to know every prior `order_by` call, which composes poorly.
//!
//! [`QuerySet::none`] is a structural short-circuit — `is_empty = true`
//! causes every terminal method (Task 6) to return the empty result without
//! a database round-trip. Useful for authorization branches
//! (`if !can_read { return qs.none(); }`) that would otherwise hit the DB
//! just to prove the obvious.
//!
//! # Variance
//!
//! `PhantomData<fn() -> T>` makes `QuerySet<T>` **covariant** in `T` and
//! ensures `Send + Sync` regardless of `T`'s own markers (the queryset
//! never owns or borrows a `T`, it merely tags which model the filters are
//! aimed at). This matches `FieldRef<M, V>`'s variance so closures that
//! take `T::Fields` and return `Condition` compose without lifetime gymnastics.
//!
//! # How (user surface)
//!
//! ```ignore
//! use djogi::prelude::*;
//!
//! let qs = Post::objects()
//!     .filter(|f| f.published.eq(true))
//!     .exclude(|f| f.title.eq("draft".to_string()))
//!     .order_by(|f| f.view_count.desc())
//!     .limit(20);
//! // Nothing has hit the DB yet — terminal methods (Task 6) do that.
//! ```

use crate::model::Model;
use crate::pg::decode::FromJoinedPgRow;
use crate::query::condition::Condition;
use crate::query::field::FieldRef;
use crate::query::order::OrderExpr;
use crate::relation::path::RelationPath;
use crate::relation::prefetch::{ErasedPrefetch, prefetch_loader};
use crate::relation::select_related::{ErasedSelectRelated, child_descriptor, join_decoder};
use std::hash::Hash;
use std::marker::PhantomData;

/// `DISTINCT` mode for a QuerySet.
///
/// `None` emits a plain `SELECT ...`. `Plain` emits `SELECT DISTINCT ...`.
/// `On(cols)` emits `SELECT DISTINCT ON (col_a, col_b) ...` — the Postgres
/// extension that keeps the first row per `(col_a, col_b)` tuple, where
/// "first" is determined by the query's `ORDER BY`.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub enum DistinctMode {
    /// `SELECT ...` — no DISTINCT clause.
    #[default]
    None,
    /// `SELECT DISTINCT ...`.
    Plain,
    /// `SELECT DISTINCT ON (col_a, col_b) ...` — Postgres extension.
    /// Column names are macro-baked `&'static str` literals, never user input.
    On(Vec<&'static str>),
}

/// Lazy query builder. Nothing hits the database until a terminal method
/// (added in Task 6) is called.
///
/// See the module-level documentation for design rationale, variance, and
/// short-circuit semantics.
pub struct QuerySet<T: Model> {
    /// Accumulated filter tree. Starts as [`Condition::True`] — the vacuous
    /// identity — and grows via AND as `filter`/`exclude` are chained.
    pub(crate) condition: Condition,
    /// Ordering expressions in emission order. `order_by` appends; it does
    /// not replace.
    pub(crate) ordering: Vec<OrderExpr>,
    /// DISTINCT mode — see [`DistinctMode`].
    pub(crate) distinct: DistinctMode,
    /// SQL `LIMIT` — `None` means no limit. `i64` to match Postgres.
    pub(crate) limit: Option<i64>,
    /// SQL `OFFSET` — `None` means no offset. `i64` to match Postgres.
    pub(crate) offset: Option<i64>,
    // EMPTY CONTRACT: every terminal method — `fetch_all`, `fetch_one`,
    // `count`, `exists`, `first`, `update`, `delete` — MUST check
    // `self.is_empty` first and return the empty result (empty `Vec`,
    // `None`, `0`, `false`, `0 rows affected`, etc.) WITHOUT issuing any
    // SQL. This is the whole point of `QuerySet::none()` — it lets
    // authorization / feature-flag branches short-circuit the DB round-
    // trip without a special-cased `if` on the caller's side.
    //
    // Grep marker: TASK6:empty_contract
    //
    /// Short-circuit flag — `true` means terminal methods return the
    /// empty result without a DB round-trip. Set only by
    /// [`QuerySet::none`].
    pub(crate) is_empty: bool,
    /// Registered prefetch paths — one entry per call to
    /// [`QuerySet::prefetch`]. Consumed by
    /// [`QuerySet::fetch_all_prefetched`](crate::query::QuerySet::fetch_all_prefetched)
    /// to run a stitching query per path after the main result set
    /// comes back. Deduplicated on registration: calling `.prefetch(path)`
    /// twice with the same [`RelationPath::source_column`] is a no-op on
    /// the second call. Kept separate from `condition`/`ordering`/etc. so
    /// a plain `.fetch_all(...)` on a queryset with prefetches compiles
    /// and behaves exactly as if no prefetch was registered — prefetches
    /// only take effect on the dedicated terminal.
    pub(crate) prefetch_paths: Vec<ErasedPrefetch>,
    /// Registered `select_related` paths — one entry per call to
    /// [`QuerySet::select_related`]. Consumed by
    /// [`QuerySet::fetch_all_joined`](crate::query::QuerySet::fetch_all_joined)
    /// to emit `LEFT JOIN` clauses and aliased child columns on the main
    /// query (no follow-up round trips — that's the whole point of
    /// `select_related` over `prefetch`). Deduplicated on registration
    /// by `source_column` in the same way `prefetch_paths` is: a second
    /// `.select_related(path)` for the same source column is a no-op.
    /// Kept separate from `prefetch_paths` because the two emission
    /// strategies are structurally different — one expands the
    /// `SELECT` list + adds a JOIN, the other fans out into a per-
    /// path follow-up query — and combining them would leak that
    /// distinction into the type.
    pub(crate) select_related_paths: Vec<ErasedSelectRelated>,
    /// Row-level lock mode — Phase 4 Task 7. Default [`LockMode::None`]
    /// emits no tail; the three `ForUpdate*` variants append `FOR
    /// UPDATE [NOWAIT|SKIP LOCKED]` to the SELECT. See
    /// [`crate::query::lock`] for the full behaviour table and the
    /// pool-backed footgun note.
    pub(crate) lock: crate::query::lock::LockMode,
    /// Covariant `T` tag; never owns or borrows a `T`.
    _model: PhantomData<fn() -> T>,
}

impl<T: Model> Clone for QuerySet<T> {
    fn clone(&self) -> Self {
        QuerySet {
            condition: self.condition.clone(),
            ordering: self.ordering.clone(),
            distinct: self.distinct.clone(),
            limit: self.limit,
            offset: self.offset,
            is_empty: self.is_empty,
            // `ErasedPrefetch: Clone` (shallow clone — fn pointers and
            // `&'static str` are trivially copyable). Cloning preserves
            // prefetch registrations across any `if`/`else` branch that
            // keeps a partially-built queryset around.
            prefetch_paths: self.prefetch_paths.clone(),
            // `ErasedSelectRelated: Clone` for the same reason — the
            // struct carries only `&'static str`, a static slice, and
            // a fn pointer, so cloning is bit-copy-cheap.
            select_related_paths: self.select_related_paths.clone(),
            // `LockMode` is `Copy` — bit-copy is trivial.
            lock: self.lock,
            _model: PhantomData,
        }
    }
}

impl<T: Model> std::fmt::Debug for QuerySet<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuerySet")
            .field("table", &T::table_name())
            .field("condition", &self.condition)
            .field("ordering", &self.ordering)
            .field("distinct", &self.distinct)
            .field("limit", &self.limit)
            .field("offset", &self.offset)
            .field("is_empty", &self.is_empty)
            .field("prefetch_paths", &self.prefetch_paths)
            .field("select_related_paths", &self.select_related_paths)
            .field("lock", &self.lock)
            .finish()
    }
}

impl<T: Model> Default for QuerySet<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Model> QuerySet<T> {
    /// Construct an empty QuerySet. Prefer `T::objects()` at call sites —
    /// it is the idiomatic spelling and reads as "all objects of this
    /// model (before filtering)".
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn new() -> Self {
        QuerySet {
            condition: Condition::True,
            ordering: Vec::new(),
            distinct: DistinctMode::None,
            limit: None,
            offset: None,
            is_empty: false,
            prefetch_paths: Vec::new(),
            select_related_paths: Vec::new(),
            lock: crate::query::lock::LockMode::None,
            _model: PhantomData,
        }
    }

    /// Structural empty QuerySet — every terminal method (Task 6) short-
    /// circuits to the empty result without touching the database.
    ///
    /// Takes `self` as an instance transform (matching Django's
    /// `queryset.none()` ergonomics) so `Post::objects().none()` compiles
    /// and reads naturally. Any filters / ordering / limits already
    /// accumulated on `self` are discarded — the returned queryset is a
    /// fresh [`QuerySet::new()`] with `is_empty = true`. From-scratch
    /// construction is spelled `QuerySet::<T>::new().none()`.
    ///
    /// Useful for authorization / feature-flag branches:
    ///
    /// ```ignore
    /// let qs = if user.is_authenticated {
    ///     Post::objects().filter(|f| f.published.eq(true))
    /// } else {
    ///     Post::objects().none()
    /// };
    /// ```
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn none(self) -> Self {
        // `self` is intentionally ignored — `.none()` is a structural reset,
        // not a conjunction with existing filters. Returning a fresh empty-
        // flagged QuerySet keeps the semantics obvious: "no matter what was
        // chained before, this matches zero rows."
        let _ = self;
        let mut qs = Self::new();
        qs.is_empty = true;
        qs
    }

    /// Add a typed filter closure to the condition tree, AND-ed with whatever
    /// already accumulated. The closure receives a default-constructed
    /// `T::Fields` (a ZST) and returns a [`Condition`].
    ///
    /// ```ignore
    /// Post::objects().filter(|f| f.published.eq(true))
    /// ```
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn filter<F>(mut self, f: F) -> Self
    where
        F: FnOnce(T::Fields) -> Condition,
    {
        let cond = f(T::Fields::default());
        self.condition = Condition::and(self.condition, cond);
        self
    }

    /// AND an expression-IR predicate onto the condition tree.
    ///
    /// The closure receives a default-constructed `T::Fields` handle
    /// and must return an [`Expr<bool>`](crate::expr::Expr) — i.e. a
    /// comparison produced by the `eq` / `neq` / `gt` / `gte` / `lt` /
    /// `lte` methods on `Expr<T>`. The returned expression is wrapped
    /// in [`Condition::Expr`] and AND-ed onto `self.condition`; the
    /// SQL emitter walks the expression via
    /// [`crate::expr::sql::emit_expr`] instead of the Phase 2
    /// column-vs-literal leaf emitter.
    ///
    /// # When to reach for `filter_expr` over `filter`
    ///
    /// [`QuerySet::filter`] (Phase 2) accepts predicates where the RHS
    /// is always a literal — `f.balance.lt(100i64)`. `filter_expr`
    /// generalises both sides: either operand can be a column ref, a
    /// literal, or an arithmetic expression. Use it for:
    ///
    /// - Field-vs-field comparisons (`balance < overdraft_limit`).
    /// - Arithmetic predicates (`balance + pending_credit > 0`).
    /// - Predicates that build on [`crate::expr::Expr`] composition —
    ///   future tasks extend this surface with aggregates, subqueries,
    ///   and `CASE` (Phase 4 Tasks 4/5).
    ///
    /// The two methods compose: a queryset may have any mix of
    /// `filter` and `filter_expr` clauses, and every call is AND-ed
    /// onto the same tree.
    ///
    /// ```ignore
    /// use djogi::prelude::*;
    ///
    /// let overdrawn = Account::objects()
    ///     .filter_expr(|f| f.balance().as_expr().lt(f.overdraft_limit().as_expr()))
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn filter_expr<F>(mut self, f: F) -> Self
    where
        F: FnOnce(T::Fields) -> crate::expr::Expr<bool>,
    {
        let expr = f(T::Fields::default());
        self.condition = Condition::and(self.condition, Condition::Expr(expr));
        self
    }

    /// Add a typed filter closure **negated** (wrapped in SQL `NOT`), AND-ed
    /// onto the existing tree. Equivalent to Django's `QuerySet.exclude()`.
    ///
    /// ```ignore
    /// Post::objects().exclude(|f| f.title.eq("draft".to_string()))
    /// ```
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn exclude<F>(mut self, f: F) -> Self
    where
        F: FnOnce(T::Fields) -> Condition,
    {
        let cond = f(T::Fields::default());
        self.condition = Condition::and(self.condition, Condition::not(cond));
        self
    }

    /// AND a programmatic filter struct onto the condition tree.
    ///
    /// The filter's accumulated clauses are folded into a single
    /// `Condition::And(...)` and AND-ed onto `self.condition`. Empty
    /// filters short-circuit — no AND-ing, no vacuous `TRUE` sub-tree.
    /// Single-clause filters unwrap to a plain `Condition::Leaf` so the
    /// SQL emitter renders `col = $1` rather than `(col = $1)`.
    ///
    /// This is the closure-free sibling of [`QuerySet::filter`] — the
    /// two paths produce structurally equivalent condition trees for the
    /// same set of lookups, and the SQL emitter treats them identically.
    /// Use this method from shell bindings, admin UIs, and any dynamic
    /// assembler that can't write a `|f|` closure at compile time.
    ///
    /// ```ignore
    /// let filter = PostFilter::new()
    ///     .published(Lookup::Eq(true))
    ///     .view_count(Lookup::Gte(50i32));
    /// let rows = Post::objects().filter_struct(filter).fetch_all(&pool).await?;
    /// ```
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn filter_struct<F: crate::query::filter::ModelFilter>(mut self, filter: F) -> Self {
        let clauses = filter.into_clauses();
        if clauses.is_empty() {
            // Empty filter — don't AND `Condition::True` onto `self.condition`;
            // `Condition::and` would fold it away anyway, but early-returning
            // makes the no-op case explicit and avoids the intermediate
            // allocation.
            return self;
        }
        let folded = crate::query::filter::clauses_into_condition(clauses);
        self.condition = Condition::and(self.condition, folded);
        self
    }

    /// Append one or more ordering expressions. Later `order_by` calls
    /// **append** to the existing ordering rather than replacing it, matching
    /// Django semantics: library code can add a stable tiebreaker without
    /// clobbering the caller's primary ordering.
    ///
    /// The closure can return either a single `OrderExpr` or a
    /// `Vec<OrderExpr>` — the `Into<Vec<OrderExpr>>` bound bridges both.
    ///
    /// ```ignore
    /// Post::objects().order_by(|f| f.view_count.desc())
    /// Post::objects().order_by(|f| vec![f.published.desc(), f.title.asc()])
    /// ```
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn order_by<F, O>(mut self, f: F) -> Self
    where
        F: FnOnce(T::Fields) -> O,
        O: Into<Vec<OrderExpr>>,
    {
        let exprs: Vec<OrderExpr> = f(T::Fields::default()).into();
        // Django-style append: library code can add tiebreakers without
        // clobbering the caller's primary ordering. NOT SeaORM-style
        // replace — swapping this to `self.ordering = exprs;` silently
        // breaks any composition layer that relies on stable secondary
        // sort keys. If a replace semantic is ever needed, add a distinct
        // `reorder_by` method rather than mutating this one.
        self.ordering.extend(exprs);
        self
    }

    /// Apply SQL `LIMIT n`. Replaces any prior `limit` value.
    ///
    /// Takes `u64` at the API boundary so negative values are not
    /// representable — the builder can never be put into an invalid state.
    /// Internally stored as `Option<i64>` to match `tokio_postgres`'s BIGINT
    /// bind type; the cast is guarded by a `debug_assert!` so any pathological
    /// `n > i64::MAX` case (impossible at query scale in practice) trips
    /// in debug builds.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn limit(mut self, n: u64) -> Self {
        debug_assert!(
            n <= i64::MAX as u64,
            "QuerySet::limit(n = {n}) overflows i64 — Postgres bind type is BIGINT"
        );
        self.limit = Some(n as i64);
        self
    }

    /// Apply SQL `OFFSET n`. Replaces any prior `offset` value.
    ///
    /// Takes `u64` for the same reason as [`QuerySet::limit`] — negative
    /// offsets are meaningless and now impossible to construct.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn offset(mut self, n: u64) -> Self {
        debug_assert!(
            n <= i64::MAX as u64,
            "QuerySet::offset(n = {n}) overflows i64 — Postgres bind type is BIGINT"
        );
        self.offset = Some(n as i64);
        self
    }

    /// Append `FOR UPDATE` to the emitted SELECT — acquire an exclusive
    /// row-level lock on every selected row for the duration of the
    /// enclosing transaction.
    ///
    /// # Footgun — wrap in `atomic()`
    ///
    /// A `FOR UPDATE` lock is scoped to the active transaction. A
    /// pool-backed context auto-commits each statement, so
    /// `Post::objects().select_for_update().fetch_all(&mut pool_ctx)`
    /// acquires the lock and releases it the instant the implicit
    /// transaction closes — **no mutual exclusion** against a concurrent
    /// writer between the fetch and the subsequent `save`. Every
    /// correctness-sensitive use of `select_for_update` MUST sit inside
    /// an [`atomic()`](crate::transaction::atomic) scope.
    ///
    /// # Chaining with `nowait` / `skip_locked`
    ///
    /// Use [`QuerySet::nowait`] or [`QuerySet::skip_locked`] AFTER
    /// `select_for_update` to pick the contention behaviour. Calling
    /// either without first calling `select_for_update` promotes the
    /// lock to `FOR UPDATE NOWAIT` / `FOR UPDATE SKIP LOCKED`
    /// respectively — they imply the base lock.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn select_for_update(mut self) -> Self {
        self.lock = crate::query::lock::LockMode::ForUpdate;
        self
    }

    /// Promote the SELECT lock to `FOR UPDATE NOWAIT` — acquire the
    /// lock if available, else return immediately with Postgres
    /// SQLSTATE `55P03` (`lock_not_available`), which terminals
    /// classify as [`DjogiError::LockConflict`](crate::DjogiError::LockConflict).
    ///
    /// Callable standalone (implies `select_for_update`). Combining
    /// with [`skip_locked`](QuerySet::skip_locked) — last call wins.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn nowait(mut self) -> Self {
        self.lock = crate::query::lock::LockMode::ForUpdateNowait;
        self
    }

    /// Promote the SELECT lock to `FOR UPDATE SKIP LOCKED` — silently
    /// skip rows locked by another session and return only the
    /// unlocked rows.
    ///
    /// The idiomatic shape for work-queue consumers: multiple workers
    /// can pull jobs concurrently without blocking each other.
    /// Callable standalone (implies `select_for_update`). Combining
    /// with [`nowait`](QuerySet::nowait) — last call wins.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn skip_locked(mut self) -> Self {
        self.lock = crate::query::lock::LockMode::ForUpdateSkipLocked;
        self
    }

    /// Switch to `SELECT DISTINCT ...`. Overrides any prior `distinct_on`.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn distinct(mut self) -> Self {
        self.distinct = DistinctMode::Plain;
        self
    }

    /// Switch to Postgres' `SELECT DISTINCT ON (cols...) ...`. The closure
    /// returns either a single [`FieldRef`] or a tuple of up to six
    /// `FieldRef`s; column order matters because Postgres uses the first row
    /// per `(cols...)` tuple according to the query's `ORDER BY`.
    ///
    /// Overrides any prior `distinct`/`distinct_on`.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn distinct_on<F, R>(mut self, f: F) -> Self
    where
        F: FnOnce(T::Fields) -> R,
        R: IntoDistinctColumns,
    {
        let cols = f(T::Fields::default()).into_distinct_columns();
        self.distinct = DistinctMode::On(cols);
        self
    }

    /// Register a single-hop prefetch against `path`. The target rows
    /// are materialised in a follow-up SQL query after the main
    /// [`fetch_all_prefetched`](crate::query::QuerySet::fetch_all_prefetched)
    /// executes; each main row is wrapped in a
    /// [`PrefetchedRow<T>`](crate::relation::PrefetchedRow) that exposes
    /// its resolved targets via
    /// [`PrefetchedRow::get`](crate::relation::PrefetchedRow::get).
    ///
    /// Calling `.prefetch(path)` twice with the same path is idempotent
    /// — the second registration is a no-op by `source_column` equality.
    /// This matches the natural expectation ("I asked for the same
    /// relation twice; please don't run two queries") and makes
    /// composition-site chaining (library code + caller both registering
    /// the same prefetch) free of surprises.
    ///
    /// # Why the bounds split across `Source::Pk` and `Target::Pk`
    ///
    /// Prefetch stitches via a `LEFT JOIN` keyed on `Source::Pk`; the
    /// `IN (...)` bind needs `Source::Pk: Encode + Type`, and the
    /// HashMap that routes targets back to parents needs it `Eq + Hash
    /// + Clone`. `Target::Pk` is not used for filtering — only for the
    /// NULL-probe in the stitching query, which goes through the raw-
    /// value path and does not require any `Target::Pk` bounds beyond
    /// what [`Model`] already guarantees.
    ///
    /// `Target` itself picks up `FromRow + Clone + Unpin` so the loader
    /// can decode the `t.*` columns and mint a per-parent owned copy;
    /// see the header comment on
    /// [`crate::relation::prefetch`] for the rationale behind the Clone
    /// bound (not on `Model` itself, just on the prefetch path).
    ///
    /// ```ignore
    /// let rows: Vec<PrefetchedRow<Vehicle>> = Vehicle::objects()
    ///     .filter(|f| f.make.eq("Toyota"))
    ///     .prefetch(VehicleRelated::owner())
    ///     .fetch_all_prefetched(&pool).await?;
    ///
    /// for row in &rows {
    ///     let owner: &Owner = row.get(VehicleRelated::owner()).unwrap();
    ///     println!("{} owned by {}", row.row.make, owner.name);
    /// }
    /// ```
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn prefetch<Target>(mut self, path: RelationPath<T, Target>) -> Self
    where
        T::Pk: postgres_types::ToSql
            + for<'r> postgres_types::FromSql<'r>
            + Eq
            + Hash
            + Clone
            + Send
            + Sync
            + 'static,
        Target: Model + crate::pg::decode::FromPgRow + Clone + Send + Unpin + 'static,
    {
        // Idempotent registration: if a prefetch for this source column
        // is already registered, don't append a duplicate. Duplicate
        // entries would each fire their own loader, costing an extra
        // round trip for no correctness gain — and would potentially
        // overwrite each other's stitched entries in a nondeterministic
        // order. The dedup key is `source_column` because the
        // `RelationPath` type parameters pin `Source`/`Target` at the
        // type level; two paths with the same source column pointing at
        // the same target are the same relation by construction.
        if self
            .prefetch_paths
            .iter()
            .any(|p| p.source_column == path.source_column())
        {
            return self;
        }
        self.prefetch_paths.push(ErasedPrefetch {
            source_column: path.source_column(),
            parent_table: T::table_name(),
            loader: prefetch_loader::<T, Target>,
        });
        self
    }

    /// Register a single-hop `select_related` against `path`. The target
    /// rows are materialised via a `LEFT JOIN` on the main query — no
    /// follow-up round trip, unlike
    /// [`prefetch`](QuerySet::prefetch). Each registered path is
    /// consumed by
    /// [`fetch_all_joined`](crate::query::QuerySet::fetch_all_joined),
    /// which returns `Vec<JoinedRow<T>>` exposing the joined target(s)
    /// via the same typed [`RelationPath`] the caller passed in.
    ///
    /// Calling `.select_related(path)` twice with the same path is
    /// idempotent — the second registration is a no-op by
    /// [`RelationPath::source_column`] equality, matching the
    /// dedup rule on `prefetch`. No `Vec` spam, no duplicate join in
    /// the emitted SQL.
    ///
    /// # Why the bounds on `Child`
    ///
    /// The `select_related` emitter aliases every child column in the
    /// `SELECT` list under a `rel_{source_column}.{col}` prefix. The
    /// [`FromJoinedPgRow`](crate::pg::decode::FromJoinedPgRow) bound on
    /// `Child` captures the decoder that reads those aliased columns
    /// back into a concrete `Child` instance — the macro-emitted
    /// sibling of `FromRow` that takes a prefix parameter. Without
    /// this bound the emitter would have no way to decode the
    /// child side of the join.
    ///
    /// `Child: Send + Sync + 'static` is the erasure contract — the
    /// decoded child is boxed as `Box<dyn Any + Send + Sync>` so it can
    /// share storage with heterogeneous child types on a single
    /// queryset (`.select_related(owner).select_related(fuel_type)`).
    ///
    /// # Multi-relation per queryset
    ///
    /// Multiple `.select_related(...)` calls on the same queryset stack —
    /// each produces its own `LEFT JOIN` with a `rel_{source_column}`
    /// alias. Aliases never collide because source columns are unique
    /// per parent model by construction. Multi-**hop** `select_related`
    /// (chained targets) is not supported: [`RelationPath`] only
    /// carries a single hop at the type level.
    ///
    /// ```ignore
    /// let rows: Vec<JoinedRow<Vehicle>> = Vehicle::objects()
    ///     .filter(|f| f.make.eq("Tesla"))
    ///     .select_related(VehicleRelated::owner())
    ///     .fetch_all_joined(&pool).await?;
    ///
    /// for row in &rows {
    ///     let owner: &Owner = row.get(VehicleRelated::owner()).unwrap();
    ///     println!("{} owned by {}", row.row.make, owner.name);
    /// }
    /// ```
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn select_related<Child>(mut self, path: RelationPath<T, Child>) -> Self
    where
        Child: Model + FromJoinedPgRow + Send + Sync + 'static,
    {
        // Idempotent registration: if a select_related for this
        // source column is already registered, don't append a
        // duplicate. Duplicate entries would emit two identical
        // `LEFT JOIN` clauses with the same alias — Postgres would
        // raise a "table name specified more than once" error on
        // execution, turning a silent repeat into a runtime failure
        // far from the call site. The dedup key is `source_column`
        // because the `RelationPath` type parameters pin `Source` /
        // `Child` at the type level; two paths with the same source
        // column pointing at the same child are the same relation by
        // construction.
        if self
            .select_related_paths
            .iter()
            .any(|p| p.source_column == path.source_column())
        {
            return self;
        }

        self.select_related_paths.push(ErasedSelectRelated {
            source_column: path.source_column(),
            child_table: path.target_table(),
            decoder: join_decoder::<Child>,
            // `child_descriptor::<Child>` coerces to a plain `fn` pointer
            // that the SELECT-list emitter uses to read every child
            // column name. Going through the descriptor (rather than a
            // pre-projected `Vec<&'static str>`) avoids an allocation
            // per `.select_related(...)` call and lets later phases
            // pull richer metadata off the same hook.
            child_descriptor: child_descriptor::<Child>,
        });
        self
    }

    /// Structural emptiness check — `true` only for querysets built via
    /// [`QuerySet::none`]. Used by Task 6's terminal methods to short-
    /// circuit the DB round-trip.
    ///
    /// `pub(crate)` because it is an implementation detail of the terminal
    /// methods, not user-facing API; users who need "does this queryset
    /// actually match rows?" should call `.exists()`, which also runs
    /// the real SQL.
    pub(crate) fn is_empty(&self) -> bool {
        self.is_empty
    }

    /// Group this queryset by one or more key columns, transitioning into
    /// [`crate::query::grouped::GroupedQuerySet<T, K>`].
    ///
    /// The closure receives a default-constructed `T::Fields` handle and must
    /// return one `FieldRef<T, V>` (arity 1) or a tuple of `FieldRef`s (arity
    /// 2..=4) — any type that implements
    /// [`crate::query::grouped::IntoGroupKeyTuple`].
    ///
    /// `GroupedQuerySet` has no terminals. Call `.annotate(...)` on the result
    /// to attach aggregate expressions and enter the terminal-bearing
    /// [`crate::query::grouped::GroupedAnnotatedQuerySet`] state. Premature
    /// `.fetch_all` on `GroupedQuerySet` is a compile error.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let rows: Vec<(i64, i64)> = Txn::objects()
    ///     .group_by(|f| f.org_id())
    ///     .annotate(|f| f.amount().sum())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    #[must_use = "grouped queries are lazy — dropping one silently omits the query"]
    pub fn group_by<F, K>(self, f: F) -> crate::query::grouped::GroupedQuerySet<T, K>
    where
        F: FnOnce(T::Fields) -> K,
        K: crate::query::grouped::IntoGroupKeyTuple,
    {
        let keys = f(T::Fields::default());
        crate::query::grouped::GroupedQuerySet {
            qs: self,
            keys,
            grouping: crate::query::grouped::GroupingMode::Plain,
            #[cfg(feature = "spatial")]
            spatial_source: None,
            _k: std::marker::PhantomData,
        }
    }

    /// Enter grouped state with ROLLUP semantics. Emits
    /// `GROUP BY ROLLUP (<keys>)` — Postgres expands this to include all
    /// hierarchical subtotals and the grand total in one pass.
    ///
    /// This is a convenience entry point equivalent to
    /// `.group_by(f)` followed by setting the grouping mode to
    /// `GroupingMode::Rollup`. Call `.annotate(...)` on the result to
    /// attach aggregate expressions.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Emits: GROUP BY ROLLUP (org_id)
    /// // Produces subtotals per org_id plus the grand total in one query.
    /// let rows = Txn::objects()
    ///     .rollup(|f| f.org_id())
    ///     .annotate(|f| f.amount().sum())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    #[must_use = "grouped queries are lazy — dropping one silently omits the query"]
    pub fn rollup<F, K>(self, f: F) -> crate::query::grouped::GroupedQuerySet<T, K>
    where
        F: FnOnce(T::Fields) -> K,
        K: crate::query::grouped::IntoGroupKeyTuple,
    {
        let mut gq = self.group_by(f);
        gq.grouping = crate::query::grouped::GroupingMode::Rollup;
        gq
    }

    /// Enter grouped state with CUBE semantics. Emits
    /// `GROUP BY CUBE (<keys>)` — Postgres expands this to all 2^n subsets
    /// of the key columns, covering every possible grouping combination.
    ///
    /// This is a convenience entry point equivalent to
    /// `.group_by(f)` followed by setting the grouping mode to
    /// `GroupingMode::Cube`. Call `.annotate(...)` on the result to
    /// attach aggregate expressions.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Emits: GROUP BY CUBE (org_id, region_id)
    /// // Produces subtotals for (org_id, region_id), (org_id), (region_id),
    /// // and the grand total — all four combinations.
    /// let rows = Txn::objects()
    ///     .cube(|f| (f.org_id(), f.region_id()))
    ///     .annotate(|f| f.amount().sum())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    #[must_use = "grouped queries are lazy — dropping one silently omits the query"]
    pub fn cube<F, K>(self, f: F) -> crate::query::grouped::GroupedQuerySet<T, K>
    where
        F: FnOnce(T::Fields) -> K,
        K: crate::query::grouped::IntoGroupKeyTuple,
    {
        let mut gq = self.group_by(f);
        gq.grouping = crate::query::grouped::GroupingMode::Cube;
        gq
    }

    /// Enter grouped state with GROUPING SETS semantics. Takes a closure
    /// that returns `[&'static str; N]` — each element becomes one
    /// single-column grouping set. Emits `GROUP BY GROUPING SETS ((col_a),
    /// (col_b), ...)`.
    ///
    /// The key type is `()` — there are no statically-typed key columns to
    /// decode because each row's "key" depends on which grouping set matched.
    /// Call `.annotate(...)` on the result to attach aggregate expressions;
    /// grouping-set column values are accessible via raw row access on the
    /// rows returned by `.fetch_all`.
    ///
    /// Arity-1 per set (one column per set). Multi-column sets are a
    /// future extension.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Emits: GROUP BY GROUPING SETS ((org_id), (region))
    /// // Each result row is grouped by exactly one of the listed columns.
    /// let rows = Txn::objects()
    ///     .group_by_sets(|_| ["org_id", "region"])
    ///     .annotate(|f| f.amount().sum())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    #[must_use = "grouped queries are lazy — dropping one silently omits the query"]
    pub fn group_by_sets<F, const N: usize>(
        self,
        f: F,
    ) -> crate::query::grouped::GroupedQuerySet<T, ()>
    where
        F: FnOnce(T::Fields) -> [&'static str; N],
    {
        let cols = f(T::Fields::default());
        let sets: Vec<Vec<&'static str>> = cols.iter().map(|c| vec![*c]).collect();
        crate::query::grouped::GroupedQuerySet {
            qs: self,
            keys: (),
            grouping: crate::query::grouped::GroupingMode::Sets(sets),
            #[cfg(feature = "spatial")]
            spatial_source: None,
            _k: std::marker::PhantomData,
        }
    }

    // ── Tree-recursive transitions (Phase 8-Zero Cluster B2 — T9) ───────────
    //
    // `tree_descendants` / `tree_ancestors` consume the queryset and
    // return a [`RecursiveQuerySet<T>`], which has its own filter /
    // ordering / search-mode builders and its own terminals. The
    // accumulated `condition` / `ordering` / `limit` / etc. on `self`
    // are intentionally **discarded**: a recursive walk is anchored
    // by `id = $root`, and additional filters / orderings only make
    // sense when re-applied through `RecursiveQuerySet`'s builder
    // surface (where the emitter knows whether a predicate goes into
    // the recursive term's `WHERE` or the outer projection's `ORDER
    // BY`). The `let _ = self;` makes the discard explicit.

    /// Walk the self-FK chain downward — every row whose ancestor
    /// chain reaches `root_id`. Returns a typed [`RecursiveQuerySet<T>`]
    /// that carries `tree_descendants` semantics (recursive term:
    /// `child.<edge_col> = parent.id`).
    ///
    /// Works for **any** model `T` with at least one self-FK edge,
    /// without requiring `#[model(tree_edge = "...")]`. The caller
    /// supplies a typed [`RelationPath<T, T>`] picked from the
    /// macro-emitted `{T}Related::<edge>()` accessor — the type-level
    /// pinning means a `RelationPath<Vehicle, Vehicle>` cannot be
    /// passed where the queryset is over `Post`.
    ///
    /// For models that declare `#[model(tree_edge = "...")]`, prefer
    /// the inherent sugar [`Model::tree_descendants`] which resolves
    /// the column from the descriptor automatically.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn tree_descendants(
        self,
        edge: crate::relation::RelationPath<T, T>,
        root_id: T::Pk,
    ) -> crate::query::recursive::RecursiveQuerySet<T>
    where
        T::Pk: postgres_types::ToSql + Sync + Send + 'static,
    {
        // Discard accumulated state — a recursive walk is anchored by
        // root_id and rebuilds its own filter / ordering pipeline.
        let _ = self;
        crate::query::recursive::RecursiveQuerySet::from_path(
            edge,
            root_id,
            crate::query::recursive::RecursiveDirection::Descendants,
        )
    }

    /// Walk the self-FK chain upward — every row reached by following
    /// the FK from `node_id` toward the root. Sibling of
    /// [`tree_descendants`](Self::tree_descendants) with the recursive
    /// term flipped to `parent.<edge_col> = child.id`.
    ///
    /// Same descriptor / typed-path requirements as `tree_descendants`;
    /// the inherent sugar [`Model::tree_ancestors`] is the
    /// `tree_edge`-declared shortcut.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn tree_ancestors(
        self,
        edge: crate::relation::RelationPath<T, T>,
        node_id: T::Pk,
    ) -> crate::query::recursive::RecursiveQuerySet<T>
    where
        T::Pk: postgres_types::ToSql + Sync + Send + 'static,
    {
        let _ = self;
        crate::query::recursive::RecursiveQuerySet::from_path(
            edge,
            node_id,
            crate::query::recursive::RecursiveDirection::Ancestors,
        )
    }

    /// Spatial LEFT JOIN GROUP BY: group rows by which region of `R` contains
    /// them.
    ///
    /// Emits:
    ///
    /// ```sql
    /// SELECT r.<pk-col> AS rk0, <aggregates>
    /// FROM <t-table> AS t
    /// LEFT JOIN <r-table> AS r ON ST_Contains(r.<r-geo-col>, t.<t-geo-col>)
    /// GROUP BY r.<pk-col>
    /// ```
    ///
    /// The `LEFT JOIN` gives users the unassigned bucket —
    /// `RegionKey { region_pk: None }` — so rows that fall outside all known
    /// regions are visible in the result, not silently dropped (which an
    /// `INNER JOIN` would do).
    ///
    /// ## Runtime warning
    ///
    /// If `R` has no GiST index on its geography column, this method warns
    /// once per process via `tracing::warn!`. A spatial JOIN without a GiST
    /// index performs a full table scan on `R` for every row in `T`, scaling
    /// as O(|T| × |R|). Add `#[model(index = ...)]` on the region model's
    /// geography field or declare an `IndexSpec` with `IndexType::Gist`.
    ///
    /// ## Type parameters
    ///
    /// - `F` — closure that picks the geography column on `T`.
    /// - `G` — the concrete geography type (e.g. `GeoPoint`, `Polygon`).
    /// - `R` — the region model. Must have at least one `Geography`-typed field
    ///   in its descriptor.
    ///
    /// ## Panics
    ///
    /// Panics at call time if `R`'s descriptor contains no
    /// `FieldSqlType::Geography` field. This is a programming error (missing
    /// geo column on the region model), not a runtime condition, so a panic is
    /// appropriate — the same way an out-of-bounds slice index is.
    #[cfg(feature = "spatial")]
    #[must_use = "grouped queries are lazy — dropping one silently omits the query"]
    pub fn group_by_region<F, G, R>(
        self,
        field: F,
        _regions: QuerySet<R>,
    ) -> crate::query::grouped::GroupedQuerySet<T, crate::query::spatial_grouping::RegionKey<R>>
    where
        F: FnOnce(T::Fields) -> crate::query::field::FieldRef<T, G>,
        G: crate::geo::GeographyValue,
        R: Model,
        R::Pk: for<'a> postgres_types::FromSql<'a> + Send + Unpin + 'static,
    {
        // ── Missing-GiST warning ─────────────────────────────────────────────
        // Fire once per process via `std::sync::Once` so logs aren't flooded
        // even when the same queryset is constructed in a hot loop.
        if !R::descriptor().has_gist_on_geography() {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(
                    target: "djogi::spatial",
                    model = R::table_name(),
                    "group_by_region called against a region model with no GiST index on a \
                     geography column; spatial JOINs without GiST scale linearly in both \
                     table sizes — add IndexType::Gist on the region model's geography field \
                     or declare an IndexSpec with extension_dependency = Some(\"postgis\")"
                );
            });
        }

        // ── Identify the data-side geo column ────────────────────────────────
        let t_geo_col = field(T::Fields::default()).column();

        // ── Identify the region-side geo column ──────────────────────────────
        // Walk the region model's descriptor to find the first Geography-typed
        // field. Panics if none exists — that is a programming error (the
        // caller named a non-spatial model as the region model).
        let r_geo_col = R::descriptor()
            .fields
            .iter()
            .find(|f| {
                matches!(
                    f.sql_type,
                    crate::descriptor::FieldSqlType::Geography { .. }
                )
            })
            .map(|f| f.name)
            .expect(
                "region model R must have at least one Geography-typed field; \
                 add a GeoPoint / Polygon / … field before calling group_by_region",
            );

        // ── PK column for the region model ───────────────────────────────────
        let r_pk_col = R::descriptor()
            .pk_column()
            .expect("region model R must have a primary key");

        // ── Build the spatial join spec ───────────────────────────────────────
        let spec = crate::query::spatial_grouping::SpatialJoinSpec {
            t_geo_col,
            r_table: R::table_name(),
            r_geo_col,
            r_pk_col,
        };

        let keys = crate::query::spatial_grouping::RegionKey::<R> {
            region_pk: None,
            r_pk_col: Some(r_pk_col),
            _phantom: std::marker::PhantomData,
        };

        crate::query::grouped::GroupedQuerySet {
            qs: self,
            keys,
            grouping: crate::query::grouped::GroupingMode::Plain,
            spatial_source: Some(crate::query::grouped::SpatialGroupSource::Join(spec)),
            _k: std::marker::PhantomData,
        }
    }

    /// Sugar for `group_by_region(..).annotate(|_| id_field.count_star())`.
    ///
    /// Returns a `GroupedAnnotatedQuerySet` that counts rows per region
    /// (including an unassigned bucket for rows outside all regions).
    ///
    /// The count aggregate is `COUNT(*)` on the data table alias `t`.
    /// Callers who need a different aggregate (e.g. `SUM(amount)`) use
    /// `group_by_region` directly and call `.annotate` themselves.
    ///
    /// ## Type parameters
    ///
    /// Same bounds as [`QuerySet::group_by_region`].
    #[cfg(feature = "spatial")]
    #[must_use = "grouped queries are lazy — dropping one silently omits the query"]
    pub fn count_by_region<F, G, R>(
        self,
        field: F,
        regions: QuerySet<R>,
    ) -> crate::query::grouped::GroupedAnnotatedQuerySet<
        T,
        crate::query::spatial_grouping::RegionKey<R>,
        crate::expr::AggregateExpr<i64>,
    >
    where
        F: FnOnce(T::Fields) -> crate::query::field::FieldRef<T, G>,
        G: crate::geo::GeographyValue,
        R: Model,
        R::Pk: for<'a> postgres_types::FromSql<'a> + Send + Unpin + 'static,
    {
        // Build a `count_star` aggregate on the `id` column as the proxy
        // receiver — the aggregate emitter uses `COUNT(*)` regardless of
        // the column name (see `AggOp::CountStar` emitter in expr/sql.rs).
        self.group_by_region(field, regions)
            .annotate(|_| crate::query::field::FieldRef::<T, i64>::new("id").count_star())
    }

    /// DBSCAN density-based clustering: group nearby points into clusters
    /// without specifying the number of clusters in advance.
    ///
    /// Emits:
    ///
    /// ```sql
    /// SELECT ST_ClusterDBSCAN(t.<col>::geometry, $eps, $minpoints) OVER () AS cluster_id,
    ///        <aggregates>
    /// FROM <table> AS t
    /// [WHERE ...]
    /// GROUP BY cluster_id
    /// ```
    ///
    /// `cluster_id = NULL` for noise points (isolated rows with fewer than
    /// `minpoints` neighbours within `eps`). With the default `min_points = 1`
    /// every row is a core point of its own cluster, so noise never appears.
    ///
    /// # Filter ordering
    ///
    /// SQL evaluates `WHERE` before window functions, so `.filter(...)` calls
    /// chained *before* `cluster_by_proximity` prune points **from the DBSCAN
    /// input** — the clustering sees only the survivors, which can produce
    /// different cluster ids than clustering the full set and filtering
    /// after. Use `.having(...)` (evaluated after `GROUP BY cluster_id`) when
    /// you want to filter on aggregate output without changing which rows
    /// participate in the clustering.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let counts = Store::objects()
    ///     .cluster_by_proximity(|f| f.location(), ClusterRadius::meters(500.0).min_points(3))
    ///     .annotate(|f| f.id.count_star())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    ///
    /// # Type parameters
    ///
    /// - `F` — closure that resolves the geography column from `T::Fields`.
    /// - `G` — concrete geography type (e.g. `GeoPoint`, `Polygon`).
    #[cfg(feature = "spatial")]
    #[must_use = "grouped queries are lazy — dropping one silently omits the query"]
    pub fn cluster_by_proximity<F, G>(
        self,
        field: F,
        radius: crate::query::spatial_grouping::ClusterRadius,
    ) -> crate::query::grouped::GroupedQuerySet<T, crate::query::spatial_grouping::ClusterId>
    where
        F: FnOnce(T::Fields) -> crate::query::field::FieldRef<T, G>,
        G: crate::geo::GeographyValue,
    {
        let t_geo_col = field(T::Fields::default()).column();
        let spec = crate::query::spatial_grouping::ClusterSpec {
            t_geo_col,
            eps_degrees: radius.eps_degrees,
            minpoints: radius.minpoints,
        };
        crate::query::grouped::GroupedQuerySet {
            qs: self,
            keys: crate::query::spatial_grouping::ClusterId(None),
            grouping: crate::query::grouped::GroupingMode::Plain,
            spatial_source: Some(crate::query::grouped::SpatialGroupSource::Cluster(spec)),
            _k: std::marker::PhantomData,
        }
    }

    /// Geohash grid bucketing: assign each row to a spatial cell of the
    /// chosen [`GeohashPrecision`] and group by that cell.
    ///
    /// Emits:
    ///
    /// ```sql
    /// SELECT ST_GeoHash(t.<col>::geometry, $precision) AS geohash, <aggregates>
    /// FROM <table> AS t
    /// [WHERE ...]
    /// GROUP BY geohash
    /// ```
    ///
    /// Geohash strings are prefix-ordered: a `P5` key is a prefix of any
    /// `P6` key that falls in the same parent cell — coarser re-aggregation
    /// is possible via string truncation without re-querying. Nullable
    /// geography columns (`Option<G>`) bucket NULLs into `GeohashKey(None)`;
    /// non-nullable columns never produce `None`.
    ///
    /// # Filter ordering
    ///
    /// SQL evaluates `WHERE` before the `ST_GeoHash` projection, so
    /// `.filter(...)` calls chained *before* `bucket_by_cell` prune points
    /// **from the bucketing input** — only the survivors are assigned a
    /// geohash and aggregated. Use `.having(...)` (evaluated after
    /// `GROUP BY geohash`) when you want to filter on aggregate output
    /// without affecting which rows are bucketed.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let heatmap = Store::objects()
    ///     .bucket_by_cell(|f| f.location(), GeohashPrecision::P5)
    ///     .annotate(|f| f.id.count_star())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    ///
    /// # Type parameters
    ///
    /// - `F` — closure that resolves the geography column from `T::Fields`.
    /// - `G` — concrete geography type (e.g. `GeoPoint`).
    #[cfg(feature = "spatial")]
    #[must_use = "grouped queries are lazy — dropping one silently omits the query"]
    pub fn bucket_by_cell<F, G>(
        self,
        field: F,
        precision: crate::query::spatial_grouping::GeohashPrecision,
    ) -> crate::query::grouped::GroupedQuerySet<T, crate::query::spatial_grouping::GeohashKey>
    where
        F: FnOnce(T::Fields) -> crate::query::field::FieldRef<T, G>,
        G: crate::geo::GeographyValue,
    {
        let t_geo_col = field(T::Fields::default()).column();
        let spec = crate::query::spatial_grouping::GeohashSpec {
            t_geo_col,
            precision: precision.as_i32(),
        };
        crate::query::grouped::GroupedQuerySet {
            qs: self,
            keys: crate::query::spatial_grouping::GeohashKey(None),
            grouping: crate::query::grouped::GroupingMode::Plain,
            spatial_source: Some(crate::query::grouped::SpatialGroupSource::Geohash(spec)),
            _k: std::marker::PhantomData,
        }
    }
}

/// Private marker trait used to seal [`IntoDistinctColumns`].
///
/// Only types djogi itself blesses — `FieldRef` and tuples of
/// `FieldRef` up to arity 6 — implement `Sealed`, and because the
/// module is crate-private, downstream crates cannot add their own
/// impls. That closes the identifier-smuggling route a hostile
/// downstream would otherwise have via
/// `impl IntoDistinctColumns for MyStruct { fn into_distinct_columns(self) -> Vec<&'static str> { vec!["1; DROP TABLE ..."] } }`.
mod distinct_seal {
    pub trait Sealed {}
}

/// Bridge from closure return types to the `Vec<&'static str>` of column
/// names that [`QuerySet::distinct_on`] stores. Implemented for
/// [`FieldRef`] and tuples of `FieldRef`s up to arity 6.
///
/// Expanding beyond six requires adding another
/// `impl_into_distinct_columns_tuple!` invocation below — the ceiling is
/// deliberately low because `DISTINCT ON` with more than a handful of
/// columns is a design smell, not a capacity limit Djogi cares to lift.
///
/// The trait is sealed via [`distinct_seal::Sealed`] — downstream code
/// can name `IntoDistinctColumns` as a bound (so `distinct_on` callers
/// can pass the ZST tuples returned by macro-generated field
/// accessors) but cannot implement it for their own types. Every
/// column name that reaches `into_distinct_columns` therefore traces
/// back to a sealed `FieldRef` whose constructor ran through
/// [`crate::ident::assert_plain_ident`].
pub trait IntoDistinctColumns: distinct_seal::Sealed {
    /// Flatten the receiver into the ordered list of column names Postgres
    /// will dedupe on.
    fn into_distinct_columns(self) -> Vec<&'static str>;
}

impl<M: Model, V> distinct_seal::Sealed for FieldRef<M, V> {}
impl<M: Model, V> IntoDistinctColumns for FieldRef<M, V> {
    fn into_distinct_columns(self) -> Vec<&'static str> {
        vec![self.column()]
    }
}

/// Generate `IntoDistinctColumns` (plus the sealed marker) for a tuple
/// of `FieldRef`s. Each type parameter stands for the tuple slot's
/// value type `V` — the model type `M` is shared across every
/// `FieldRef` in the tuple because `distinct_on` only ever sees one
/// model's columns at a time.
macro_rules! impl_into_distinct_columns_tuple {
    ($($name:ident),+) => {
        impl<M: Model, $($name),+> distinct_seal::Sealed for ($(FieldRef<M, $name>,)+) {}
        impl<M: Model, $($name),+> IntoDistinctColumns for ($(FieldRef<M, $name>,)+) {
            fn into_distinct_columns(self) -> Vec<&'static str> {
                #[allow(non_snake_case)]
                let ($($name,)+) = self;
                vec![$($name.column()),+]
            }
        }
    };
}
impl_into_distinct_columns_tuple!(A);
impl_into_distinct_columns_tuple!(A, B);
impl_into_distinct_columns_tuple!(A, B, C);
impl_into_distinct_columns_tuple!(A, B, C, D);
impl_into_distinct_columns_tuple!(A, B, C, D, E);
impl_into_distinct_columns_tuple!(A, B, C, D, E, F);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::ModelDescriptor;

    // Minimal `Model` impl for builder-shape tests. Mirrors the `Fake` model
    // used in `query::field`'s unit tests — keeps QuerySet builder tests in
    // this file independent of the `#[model]` macro expansion path.
    struct Fake;
    impl crate::model::__sealed::Sealed for Fake {}
    #[allow(clippy::manual_async_fn)]
    impl Model for Fake {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "fake"
        }
        fn pk_value(&self) -> &Self::Pk {
            unreachable!("not called in QuerySet unit tests")
        }
        fn descriptor() -> &'static ModelDescriptor {
            unreachable!("not called in QuerySet unit tests")
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: Self::Pk,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
    }

    #[test]
    fn new_queryset_has_no_filters() {
        let qs: QuerySet<Fake> = QuerySet::new();
        assert!(matches!(qs.condition, Condition::True));
        assert!(qs.ordering.is_empty());
        assert!(matches!(qs.distinct, DistinctMode::None));
        assert_eq!(qs.limit, None);
        assert_eq!(qs.offset, None);
        assert!(!qs.is_empty());
    }

    #[test]
    fn none_marks_queryset_empty() {
        // `none()` is an instance method: from-scratch construction goes
        // through `new().none()`, but `qs.none()` also works and is the
        // spelling documented at the module level.
        let qs: QuerySet<Fake> = QuerySet::<Fake>::new().none();
        assert!(qs.is_empty());
    }

    #[test]
    fn none_discards_prior_filters() {
        use crate::query::condition::{FilterValue, Leaf};
        // Any filters chained before `.none()` are structurally discarded —
        // the resulting queryset is always a clean empty-flagged state.
        let qs: QuerySet<Fake> = QuerySet::new()
            .filter(|_| Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true))))
            .none();
        assert!(qs.is_empty());
        assert!(matches!(qs.condition, Condition::True));
    }

    #[test]
    fn filter_ands_onto_condition_tree() {
        use crate::query::condition::{FilterValue, Leaf};
        let qs: QuerySet<Fake> =
            QuerySet::new().filter(|_| Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true))));
        assert!(matches!(qs.condition, Condition::Leaf(_)));
        // Second filter should AND with the first.
        let qs2 = qs.filter(|_| Condition::Leaf(Leaf::eq_raw("b", FilterValue::Bool(false))));
        match qs2.condition {
            Condition::And(parts) => assert_eq!(parts.len(), 2),
            other => panic!("expected And, got {other:?}"),
        }
    }

    #[test]
    fn exclude_wraps_in_not() {
        use crate::query::condition::{FilterValue, Leaf};
        let qs: QuerySet<Fake> = QuerySet::new()
            .exclude(|_| Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true))));
        // True AND NOT(leaf) → NOT(leaf) thanks to Condition::and's identity folding.
        assert!(matches!(qs.condition, Condition::Not(_)));
    }

    #[test]
    fn limit_and_offset_round_trip() {
        let qs: QuerySet<Fake> = QuerySet::new().limit(10).offset(20);
        assert_eq!(qs.limit, Some(10));
        assert_eq!(qs.offset, Some(20));
    }

    #[test]
    fn distinct_plain_sets_mode() {
        let qs: QuerySet<Fake> = QuerySet::new().distinct();
        assert!(matches!(qs.distinct, DistinctMode::Plain));
    }

    #[test]
    fn clone_is_structural() {
        let qs: QuerySet<Fake> = QuerySet::new().limit(5);
        let qs2 = qs.clone();
        assert_eq!(qs.limit, qs2.limit);
    }

    // ── T11: group_by_region / count_by_region type-dispatch tests ────────────
    //
    // These tests confirm the entry-point signatures compile and return the
    // expected type shapes. The SQL emission shape is tested in sql.rs.

    // ── P1-2 once-warn counter test ───────────────────────────────────────────
    //
    // Verifies that calling `group_by_region` against an unindexed region model
    // emits at most one `tracing::warn!` regardless of how many times the method
    // is called. The guard uses `std::sync::Once` which is process-wide; the
    // counter is captured with `tracing_test::traced_test` scoped to this test's
    // thread-local subscriber.
    //
    // `ONCE` fires at most once per process run. `#[traced_test]` captures the
    // warn if it fires inside this test invocation and zero otherwise. The
    // assertion uses `<=` rather than `==` because test parallelism may cause
    // another unindexed call (in a different test) to consume the `Once` first —
    // the bound of at most one is still the invariant being checked.

    /// A region model with a geography field but NO GiST index — triggers the
    /// once-per-process warn path in `group_by_region`.
    #[cfg(feature = "spatial")]
    struct FakeUnindexedRegion;
    #[cfg(feature = "spatial")]
    impl crate::model::__sealed::Sealed for FakeUnindexedRegion {}
    #[cfg(feature = "spatial")]
    #[allow(clippy::manual_async_fn)]
    impl Model for FakeUnindexedRegion {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "unindexed_regions"
        }
        fn pk_value(&self) -> &i64 {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            use crate::descriptor::{
                FieldDescriptor, FieldSqlType, GeographySubtype, PkType, field_descriptor,
                model_descriptor,
            };
            static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
                ..field_descriptor(
                    "boundary",
                    FieldSqlType::Geography {
                        subtype: GeographySubtype::Polygon,
                        srid: 4326,
                    },
                    false,
                )
            }];
            static DESC: ModelDescriptor = ModelDescriptor {
                ..model_descriptor(
                    "FakeUnindexedRegion",
                    "unindexed_regions",
                    PkType::HeerId,
                    FIELDS,
                )
            };
            &DESC
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: i64,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
    }

    /// Verifies the once-warn guard: `group_by_region` must emit at most one
    /// `warn!` for an unindexed region model even when called multiple times.
    ///
    /// A custom `tracing::Subscriber` counts `WARN`-level events from the
    /// `djogi::spatial` target. The `std::sync::Once` guard is process-wide,
    /// so the warn fires in whichever test invocation happens first;
    /// the counter captures it if it fires inside this test. The assertion
    /// bounds the count to `<= 1`, which is the invariant regardless of
    /// test ordering.
    #[cfg(feature = "spatial")]
    #[test]
    fn group_by_region_warns_at_most_once_for_unindexed_region() {
        use crate::geo::GeoPoint;
        use crate::query::field::FieldRef;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Per-invocation counter — never accumulates across tests because
        // each test sees its own stack-allocated `AtomicUsize`.
        let warn_count = std::sync::Arc::new(AtomicUsize::new(0));

        // Minimal `Subscriber` that counts WARN events from "djogi::spatial".
        struct WarnCountSub {
            count: std::sync::Arc<AtomicUsize>,
        }
        impl tracing::Subscriber for WarnCountSub {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                if *event.metadata().level() == tracing::Level::WARN
                    && event.metadata().target() == "djogi::spatial"
                {
                    self.count.fetch_add(1, Ordering::Relaxed);
                }
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }

        let subscriber = WarnCountSub {
            count: warn_count.clone(),
        };
        let _guard = tracing::subscriber::set_default(subscriber);

        // Call group_by_region twice — only the first call (in this process)
        // should fire the tracing::warn!. The second is a no-op via Once.
        let _g1 = QuerySet::<Fake>::new().group_by_region(
            |_| FieldRef::<Fake, GeoPoint>::new("location"),
            QuerySet::<FakeUnindexedRegion>::new(),
        );
        let _g2 = QuerySet::<Fake>::new().group_by_region(
            |_| FieldRef::<Fake, GeoPoint>::new("location"),
            QuerySet::<FakeUnindexedRegion>::new(),
        );

        // The Once guard ensures at most one warn fires across the whole process.
        // If this test runs first on the unindexed path: count == 1.
        // If another test fired the Once before us: count == 0.
        // Both satisfy the invariant "never > 1".
        let count = warn_count.load(Ordering::Relaxed);
        assert!(
            count <= 1,
            "expected at most 1 warn from group_by_region (Once guard), got {count}"
        );
    }

    /// A `FakeIndexedRegion` that returns a descriptor with a GiST index on a
    /// Geography field — `group_by_region` should NOT emit a warning.
    #[cfg(feature = "spatial")]
    struct FakeIndexedRegion;
    #[cfg(feature = "spatial")]
    impl crate::model::__sealed::Sealed for FakeIndexedRegion {}
    #[cfg(feature = "spatial")]
    #[allow(clippy::manual_async_fn)]
    impl Model for FakeIndexedRegion {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "indexed_regions"
        }
        fn pk_value(&self) -> &i64 {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            use crate::descriptor::{
                FieldDescriptor, FieldSqlType, GeographySubtype, IndexColumnSpec, IndexKind,
                IndexSpec, IndexTarget, IndexType, PkType, field_descriptor, model_descriptor,
            };
            static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
                ..field_descriptor(
                    "boundary",
                    FieldSqlType::Geography {
                        subtype: GeographySubtype::Polygon,
                        srid: 4326,
                    },
                    false,
                )
            }];
            static BOUNDARY_COLS: &[IndexColumnSpec] = &[IndexColumnSpec::simple("boundary")];
            static INDEXES: &[IndexSpec] = &[IndexSpec {
                name: "idx_regions_boundary_gist",
                target: IndexTarget::Columns(BOUNDARY_COLS),
                kind: IndexKind::NonUnique,
                index_type: IndexType::Gist,
                predicate: None,
                include: &[],
                nulls_not_distinct: false,
                requires_out_of_transaction: true,
                extension_dependency: Some("postgis"),
            }];
            static DESC: ModelDescriptor = ModelDescriptor {
                indexes: INDEXES,
                ..model_descriptor(
                    "FakeIndexedRegion",
                    "indexed_regions",
                    PkType::HeerId,
                    FIELDS,
                )
            };
            &DESC
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: i64,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
    }

    /// `group_by_region` returns a `GroupedQuerySet<T, RegionKey<R>>`.
    /// This compile-pass test verifies the type shape is as specified.
    #[cfg(feature = "spatial")]
    #[test]
    fn group_by_region_returns_grouped_queryset_with_region_key() {
        use crate::geo::GeoPoint;
        use crate::query::field::FieldRef;
        use crate::query::grouped::GroupedQuerySet;
        use crate::query::spatial_grouping::RegionKey;

        let qs: QuerySet<Fake> = QuerySet::new();
        let regions: QuerySet<FakeIndexedRegion> = QuerySet::new();
        let _grouped: GroupedQuerySet<Fake, RegionKey<FakeIndexedRegion>> =
            qs.group_by_region(|_| FieldRef::<Fake, GeoPoint>::new("location"), regions);
    }

    /// `count_by_region` returns a `GroupedAnnotatedQuerySet<T, RegionKey<R>, AggregateExpr<i64>>`.
    #[cfg(feature = "spatial")]
    #[test]
    fn count_by_region_returns_grouped_annotated_queryset() {
        use crate::expr::AggregateExpr;
        use crate::geo::GeoPoint;
        use crate::query::field::FieldRef;
        use crate::query::grouped::GroupedAnnotatedQuerySet;
        use crate::query::spatial_grouping::RegionKey;

        let qs: QuerySet<Fake> = QuerySet::new();
        let regions: QuerySet<FakeIndexedRegion> = QuerySet::new();
        let _gaq: GroupedAnnotatedQuerySet<Fake, RegionKey<FakeIndexedRegion>, AggregateExpr<i64>> =
            qs.count_by_region(|_| FieldRef::<Fake, GeoPoint>::new("location"), regions);
    }

    /// Calling `group_by_region` twice on the same model compiles — verifies
    /// that constructing two querysets does not conflict on the `Once`-based
    /// warn guard. The `std::sync::Once` is process-wide and non-resettable;
    /// the second call is a no-op without deadlock.
    #[cfg(feature = "spatial")]
    #[test]
    fn group_by_region_twice_does_not_deadlock() {
        use crate::geo::GeoPoint;
        use crate::query::field::FieldRef;

        let _g1 = QuerySet::<Fake>::new().group_by_region(
            |_| FieldRef::<Fake, GeoPoint>::new("location"),
            QuerySet::<FakeIndexedRegion>::new(),
        );
        let _g2 = QuerySet::<Fake>::new().group_by_region(
            |_| FieldRef::<Fake, GeoPoint>::new("location"),
            QuerySet::<FakeIndexedRegion>::new(),
        );
        // If we got here without a deadlock or panic, the Once guard is correct.
    }

    // ── T12: cluster_by_proximity / bucket_by_cell type-dispatch tests ────────

    /// `cluster_by_proximity` returns `GroupedQuerySet<T, ClusterId>`.
    #[cfg(feature = "spatial")]
    #[test]
    fn cluster_by_proximity_returns_grouped_queryset_with_cluster_id() {
        use crate::geo::GeoPoint;
        use crate::query::field::FieldRef;
        use crate::query::grouped::GroupedQuerySet;
        use crate::query::spatial_grouping::{ClusterId, ClusterRadius};
        // Type-level check: the return type must be `GroupedQuerySet<Fake, ClusterId>`.
        let _g: GroupedQuerySet<Fake, ClusterId> = QuerySet::<Fake>::new().cluster_by_proximity(
            |_| FieldRef::<Fake, GeoPoint>::new("location"),
            ClusterRadius::meters(500.0).min_points(3),
        );
    }

    /// `bucket_by_cell` returns `GroupedQuerySet<T, GeohashKey>`.
    #[cfg(feature = "spatial")]
    #[test]
    fn bucket_by_cell_returns_grouped_queryset_with_geohash_key() {
        use crate::geo::GeoPoint;
        use crate::query::field::FieldRef;
        use crate::query::grouped::GroupedQuerySet;
        use crate::query::spatial_grouping::{GeohashKey, GeohashPrecision};
        let _g: GroupedQuerySet<Fake, GeohashKey> = QuerySet::<Fake>::new().bucket_by_cell(
            |_| FieldRef::<Fake, GeoPoint>::new("location"),
            GeohashPrecision::P5,
        );
    }
}
