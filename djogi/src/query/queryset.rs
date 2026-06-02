//! `QuerySet<T>` — the lazy query builder.
//! # What
//! [`QuerySet<T>`] accumulates filter conditions, ordering, distinct mode,
//! and pagination (`limit` / `offset`) without hitting the database. Every
//! builder method (`filter`, `exclude`, `order_by`, `limit`, `offset`,
//! `distinct`, `distinct_on`) consumes `self` and returns `Self`, so a
//! `QuerySet` is immutable-by-convention: composition never mutates an
//! existing queryset in place.
//! `T::objects` (default method on the `Model` trait, added in Task 5)
//! constructs an empty `QuerySet<T>` — no filters, no ordering, no limit.
//! This is the entry point for every query.
//! # Why
//! Terminal methods (Task 6 — `fetch_all`, `fetch_one`, `count`, `exists`,
//! `first`, `update`, `delete`) are the **only** place SQL is generated or
//! executed. Everything else is a cheap structural transformation: `Condition`
//! trees shared across clones, small `Vec`s for ordering and distinct_on
//! column lists, POD enums for distinct mode.
//! Builder methods that append to accumulators (currently only `order_by`)
//! follow Django-style semantics: calling `.order_by(...)` twice **appends**
//! rather than replaces, so library code can add a stable tiebreaker without
//! clobbering the caller's primary ordering. Replace semantics would force
//! every caller to know every prior `order_by` call, which composes poorly.
//! [`QuerySet::none`] is a structural short-circuit — `is_empty = true`
//! causes every terminal method (Task 6) to return the empty result without
//! a database round-trip. Useful for authorization branches
//! (`if !can_read { return qs.none; }`) that would otherwise hit the DB
//! just to prove the obvious.
//! # Variance
//! `PhantomData<fn -> T>` makes `QuerySet<T>` **covariant** in `T` and
//! ensures `Send + Sync` regardless of `T`'s own markers (the queryset
//! never owns or borrows a `T`, it merely tags which model the filters are
//! aimed at). This matches `FieldRef<M, V>`'s variance so closures that
//! take `T::Fields` and return `Condition` compose without lifetime gymnastics.
//! # How (user surface)
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
use crate::query::PortablePredicateError;
use crate::query::condition::Condition;
use crate::query::field::FieldRef;
use crate::query::order::OrderExpr;
use crate::query::q::{CompoundOp, Q};
use crate::relation::path::RelationPath;
use crate::relation::prefetch::{ErasedPrefetch, prefetch_loader};
use crate::relation::select_related::{ErasedSelectRelated, child_descriptor, join_decoder};
use std::hash::Hash;
use std::marker::PhantomData;

/// `DISTINCT` mode for a QuerySet.
/// `None` emits a plain `SELECT ...`. `Plain` emits `SELECT DISTINCT ...`.
/// `On(cols)` emits `SELECT DISTINCT ON (col_a, col_b) ...` — the Postgres
/// extension that keeps the first row per `(col_a, col_b)` tuple, where
/// "first" is determined by the query's `ORDER BY`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

/// Type-erased binding from a [`QuerySet`] to a [`sassi::Punnu`] for the
/// post-fetch cache hook.
/// # What
/// Implementors carry a concrete `sassi::Punnu<T>` and expose a single
/// async [`CacheTarget::insert`] hook that the terminal methods call
/// once per fetched row. The trait is `pub(crate)` because it exists
/// purely to keep the `T: sassi::Cacheable` bound off the
/// [`QuerySet`] struct (`Punnu<T: Cacheable>` would otherwise force
/// the bound onto every `impl<T: Model> QuerySet<T>` block via rustc's
/// well-formedness check).
/// # Why a trait object, not `Option<Punnu<T>>`
/// See the doc on [`QuerySet::cache_target`] — the upshot is that
/// naming `Punnu<T>` directly in the `QuerySet` field set would
/// propagate `T: sassi::Cacheable` everywhere the struct is named.
/// Type-erasing the handle through this trait keeps that bound
/// localised to [`QuerySet::cache`], which is the only place it
/// actually matters.
/// # Why async-fn-in-trait
/// `Punnu::insert` is async (it write-throughs to any attached L2
/// backend), so the hook must be async too. The Send bound on the
/// returned future matches the `+ Send` shape every QuerySet terminal
/// already returns — terminals can `.await` the insert from any
/// multi-thread executor.
pub(crate) trait CacheTarget<T>: Send + Sync {
    /// Insert one row into the bound Punnu. Returns a `+ Send` future
    /// that resolves to `` (errors are logged and swallowed inside
    /// the implementor — see [`PunnuCacheTarget::insert`] — so the
    /// fetch terminal is never aborted by a cache-side failure).
    /// Takes `&T` (not `T`) because the terminal still owns the
    /// fetched `Vec<T>` and returns it to the caller — the cache
    /// hook is a side-effect on a borrow, not a transfer of
    /// ownership. The implementor clones internally inside the
    /// wrapper where the necessary `T: Clone` bound is satisfied
    /// (see [`PunnuCacheTarget`] — the wrapper requires
    /// `T: Cacheable + Clone`); routing the clone through the
    /// trait keeps the `T: Clone` bound off every terminal-method
    /// signature in `terminal.rs`, preserving the prior surface
    /// for adopters who never call `.cache(...)`.
    /// `&self` (not `&mut`) because `Punnu::insert` does. Returning
    /// a boxed future keeps the trait object-safe.
    fn insert<'a>(
        &'a self,
        value: &'a T,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

/// Concrete [`CacheTarget`] backed by a `sassi::Punnu<T>`.
/// # Why a wrapper, not a blanket impl on `Punnu<T>`
/// `Punnu<T>` lives in the sibling `sassi` crate; the orphan rule
/// forbids implementing a djogi-owned trait on a sassi-owned type
/// outside djogi without a wrapper. The wrapper is also where errors
/// from `Punnu::insert` get logged-and-swallowed — see the impl below
/// for why "do not propagate cache-side errors out of a fetch
/// terminal" is the load-bearing contract.
pub(crate) struct PunnuCacheTarget<T: crate::types::Cacheable> {
    punnu: sassi::Punnu<T>,
}

impl<T: crate::types::Cacheable> PunnuCacheTarget<T> {
    /// Wrap a `sassi::Punnu<T>` for use as a [`CacheTarget`].
    pub(crate) fn new(punnu: sassi::Punnu<T>) -> Self {
        Self { punnu }
    }
}

impl<T: crate::types::Cacheable + Clone> CacheTarget<T> for PunnuCacheTarget<T> {
    fn insert<'a>(
        &'a self,
        value: &'a T,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        // Clone the Punnu handle (cheap: `Arc::clone` on
        // `Arc<PunnuInner<T>>`) and the row (delegated to the user-
        // supplied `Clone` impl — required at `.cache(...)` call
        // time via the `T: Clone` bound on `QuerySet::cache`).
        // Cloning is required because `Punnu::insert(T)` takes `T`
        // by value — sassi's identity-map semantics own the
        // inserted value in an `Arc<T>` once it lands in the L1
        // snapshot.
        let punnu = self.punnu.clone();
        let value = value.clone();
        Box::pin(async move {
            // The cache-insert error contract: "Errors from `insert`
            // (e.g., `InsertError::Conflict` under `OnConflict::Reject`)
            // MUST NOT abort the fetch; log via `tracing::warn!` and
            // continue. Do NOT add `?` to the insert."
            // The terminal contract is "fetch returned rows;
            // cache-side write-through is best-effort". A conflict
            // under `OnConflict::Reject`, an L2-backend
            // serialization failure, an LRU pressure spike — none of
            // these change what Postgres said the rows are. Logging
            // the error preserves observability without breaking the
            // caller.
            if let Err(e) = punnu.insert(value).await {
                tracing::warn!(
                    target: "djogi::cache",
                    error = ?e,
                    "Punnu::insert failed during QuerySet::cache hook; continuing",
                );
            }
        })
    }
}

/// Lazy query builder. Nothing hits the database until a terminal method
/// is called.
/// See the module-level documentation for design rationale, variance, and
/// short-circuit semantics.
pub struct QuerySet<T: Model> {
    /// Accumulated filter tree. Starts as
    /// [`Q::always_true`](crate::query::Q) — the vacuous identity
    /// and grows via AND as `filter` / `exclude` / `filter_struct` /
    /// `exclude_struct` are chained.
    /// # Substrate
    /// The queryset's filter
    /// substrate is the [`Q<T>`](crate::query::Q) algebra rather than
    /// the legacy [`Condition`] tree. made SQL emission
    /// walk `Q<T>` directly: legacy [`Condition`] payloads still
    /// round-trip as `Q::Condition(_)`, while trusted portable
    /// predicates stay as `Q::Portable(_)` and emit through the
    /// portable SQL walker. Character-for-character SQL parity with the
    /// pre-flip queryset remains the load-bearing contract for legacy
    /// condition paths.
    pub(crate) condition: Q<T>,
    /// Ordering expressions in emission order. `order_by` appends; it does
    /// not replace.
    pub(crate) ordering: Vec<OrderExpr>,
    /// `true` when user code explicitly called [`QuerySet::order_by`].
    /// `QuerySet::new` may seed `ordering` from
    /// [`Model::default_order_by`], especially for proxy models.
    /// Mutating terminals should reject only *explicit* `order_by(...)`
    /// tails, not model-default ordering.
    pub(crate) has_explicit_ordering: bool,
    /// DISTINCT mode — see [`DistinctMode`].
    pub(crate) distinct: DistinctMode,
    /// SQL `LIMIT` — `None` means no limit. `i64` to match Postgres.
    pub(crate) limit: Option<i64>,
    /// SQL `OFFSET` — `None` means no offset. `i64` to match Postgres.
    pub(crate) offset: Option<i64>,
    // EMPTY CONTRACT: terminal methods return the empty result (`Vec::new`,
    // `None`, `0`, `false`, etc.) WITHOUT issuing SQL when `self.is_empty`
    // is `true`. For mutation terminals (`update`/`delete` families), this
    // short-circuit runs AFTER unsupported-state validation, matching the
    // validate-then-short-circuit contract in `insert_select.rs`.
    // This is the point of `QuerySet::none` — authorization / feature-flag
    // branches can short-circuit the DB round-trip without a special-cased
    // `if` at the call site.
    // Grep marker: TASK6:empty_contract
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
    /// Row-level lock mode — shipped the `ForUpdate*`
    /// family; added the `ForShare*` family. Default
    /// [`LockMode::None`] emits no tail; non-default variants append
    /// `FOR UPDATE` / `FOR SHARE` (optionally `NOWAIT` / `SKIP LOCKED`)
    /// to the SELECT. See [`crate::query::lock`] for the full behaviour
    /// table and the pool-backed footgun note.
    pub(crate) lock: crate::query::lock::LockMode,
    /// Optional Punnu binding — 3. When `Some(handle)`,
    /// every terminal method that produces user-facing rows
    /// (`fetch_all` / `first` / `fetch_one`) inserts each fetched row
    /// into the bound Punnu via [`sassi::Punnu::insert`] post-fetch,
    /// before returning the value to the caller. Identity-map
    /// semantics + the configured [`sassi::OnConflict`] policy from
    /// sassi apply.
    /// # Why a type-erased handle, not `Option<Punnu<T>>` directly
    /// `sassi::Punnu<T>` is defined `Punnu<T: Cacheable>`, so naming
    /// the type in a field of `QuerySet<T: Model>` would force
    /// `T: Cacheable` onto every existing `impl<T: Model> QuerySet<T>`
    /// block in the crate (the bound propagates structurally through
    /// rustc's well-formedness check). Type-erasing the binding
    /// through [`CacheTarget`] keeps the existing surface — every
    /// non-cache builder, every terminal, every Clone / Debug impl
    /// unchanged for non-cacheable `T`. The bound `T: Cacheable` is
    /// applied only on the [`QuerySet::cache`](Self::cache) builder
    /// where it actually matters.
    /// # Why `Arc`-cheap cloning
    /// `sassi::Punnu<T>` is `Arc`-internal
    /// (`sassi-reference/sassi/src/punnu/pool.rs` lines 112–122 — the
    /// struct holds a single `Arc<PunnuInner<T>>` and `Clone` clones
    /// the `Arc`). The boxed [`CacheTarget`] underneath similarly
    /// clones a single `Arc<dyn ...>` handle. Binding a queryset to a
    /// Punnu therefore records "this QuerySet feeds that Punnu
    /// instance" rather than taking a snapshot of the pool's contents.
    /// # Why outside the SQL emit path
    /// The SQL emitter ([`crate::query::sql::build_select`] and
    /// friends) never reads this field. The cache modifier is purely
    /// additive: SQL output, query plan, and the lifetime of the
    /// QuerySet are unchanged whether `cache_target` is `None` or
    /// `Some(_)`. The post-fetch hook fires exactly once per terminal
    /// call.
    pub(crate) cache_target: Option<std::sync::Arc<dyn CacheTarget<T>>>,
    /// Covariant `T` tag; never owns or borrows a `T`.
    _model: PhantomData<fn() -> T>,
}

/// A queryset whose accumulated predicate tree has been proven reducible to
/// Sassi's portable [`BasicPredicate`](sassi::BasicPredicate) algebra.
/// Constructed only by [`QuerySet::try_portable`]. The stored predicate is
/// produced by a trusted borrow-walk over `Q<T>`; there is no public
/// constructor accepting a raw Sassi predicate, so downstream code cannot
/// pair forged field names with unrelated extractors and then feed the result
/// into a cache boundary.
pub struct PortableQuerySet<T: Model> {
    inner: QuerySet<T>,
    portable_predicate: sassi::BasicPredicate<T>,
}

/// Cached terminal wrapper for a portable queryset.
/// The wrapper owns the database queryset and borrows the target Punnu only
/// long enough to clone Sassi's cheap handle before delegating to the existing
/// QuerySet terminal implementations.
pub struct CachedPortableQuerySet<'p, T: Model + crate::types::Cacheable + Clone> {
    inner: PortableQuerySet<T>,
    punnu: &'p sassi::Punnu<T>,
}

fn basic_predicate_kind<T: Model>(predicate: &sassi::BasicPredicate<T>) -> &'static str {
    match predicate {
        sassi::BasicPredicate::True => "True",
        sassi::BasicPredicate::False => "False",
        sassi::BasicPredicate::Field(_) => "Field",
        sassi::BasicPredicate::And(_) => "And",
        sassi::BasicPredicate::Or(_) => "Or",
        sassi::BasicPredicate::Not(_) => "Not",
        sassi::BasicPredicate::Xor(_, _) => "Xor",
        _ => "<unknown>",
    }
}

impl<T: Model> std::fmt::Debug for PortableQuerySet<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortableQuerySet")
            .field("inner", &self.inner)
            .field(
                "portable_predicate",
                &basic_predicate_kind(&self.portable_predicate),
            )
            .finish()
    }
}

impl<T: Model> PortableQuerySet<T> {
    /// Drop the portable-cache guarantee and return to normal SQL queryset
    /// execution.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn into_query_set(self) -> QuerySet<T> {
        self.inner
    }

    /// Borrow the underlying SQL queryset.
    pub fn as_query_set(&self) -> &QuerySet<T> {
        &self.inner
    }

    /// Borrow the Sassi predicate proven at the cache boundary.
    pub fn predicate(&self) -> &sassi::BasicPredicate<T> {
        &self.portable_predicate
    }

    /// Run the queryset and return every matching row. Equivalent to
    /// [`QuerySet::fetch_all`]; the portable wrapper delegates without
    /// rebinding any cache target so the query reaches the database
    /// unchanged. Pair with [`PortableQuerySet::cache`] when you want
    /// the returned rows mirrored into a [`sassi::Punnu`].
    pub fn fetch_all<'ctx>(
        self,
        ctx: &'ctx mut crate::DjogiContext,
    ) -> impl std::future::Future<Output = Result<Vec<T>, crate::DjogiError>> + Send + 'ctx
    where
        T: crate::pg::decode::FromPgRow + Send + Unpin + 'ctx,
    {
        self.inner.fetch_all(ctx)
    }

    /// Run the queryset and return exactly one matching row. Delegates to
    /// [`QuerySet::fetch_one`]; mirrors the same `NotFound`/`MultipleFound`
    /// errors as the underlying queryset.
    pub fn fetch_one<'ctx>(
        self,
        ctx: &'ctx mut crate::DjogiContext,
    ) -> impl std::future::Future<Output = Result<T, crate::DjogiError>> + Send + 'ctx
    where
        T: crate::pg::decode::FromPgRow + Send + Unpin + 'ctx,
    {
        self.inner.fetch_one(ctx)
    }

    /// Run the queryset and return the first matching row, or `None` if no
    /// rows match. Delegates to [`QuerySet::first`].
    pub fn first<'ctx>(
        self,
        ctx: &'ctx mut crate::DjogiContext,
    ) -> impl std::future::Future<Output = Result<Option<T>, crate::DjogiError>> + Send + 'ctx
    where
        T: crate::pg::decode::FromPgRow + Send + Unpin + 'ctx,
    {
        self.inner.first(ctx)
    }

    /// Run the queryset and return the row count. Delegates to
    /// [`QuerySet::count`]; cache binding is intentionally not applied
    /// because `COUNT(*)` does not return rows the identity map could
    /// absorb.
    pub fn count<'ctx>(
        self,
        ctx: &'ctx mut crate::DjogiContext,
    ) -> impl std::future::Future<Output = Result<i64, crate::DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
    {
        self.inner.count(ctx)
    }
}

impl<T: Model + crate::types::Cacheable + Clone> PortableQuerySet<T> {
    /// Bind a [`sassi::Punnu`] to the queryset so that rows returned by the
    /// next terminal call are mirrored into the Punnu's identity map.
    /// The returned [`CachedPortableQuerySet`] borrows `punnu` only long
    /// enough to clone Sassi's cheap `Arc`-backed handle when the terminal
    /// fires; the queryset itself stays owned. Cache binding only applies
    /// to row-returning terminals (`fetch_all`, `fetch_one`, `first`);
    /// `count` runs unchanged because it returns no rows.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn cache(self, punnu: &sassi::Punnu<T>) -> CachedPortableQuerySet<'_, T> {
        CachedPortableQuerySet { inner: self, punnu }
    }
}

impl<T: Model + crate::types::Cacheable + Clone> CachedPortableQuerySet<'_, T> {
    /// Run the queryset, return every matching row, and mirror the rows
    /// into the bound Punnu's identity map.
    /// Cache binding happens at the moment of the terminal call: the
    /// captured `&Punnu<T>` is cloned into the queryset's cache target,
    /// and the existing [`QuerySet::fetch_all`] terminal pipeline upserts
    /// rows under the same on-commit hook used elsewhere.
    pub fn fetch_all<'ctx>(
        self,
        ctx: &'ctx mut crate::DjogiContext,
    ) -> impl std::future::Future<Output = Result<Vec<T>, crate::DjogiError>> + Send + 'ctx
    where
        T: crate::pg::decode::FromPgRow + Send + Unpin + 'ctx,
    {
        let qs = self.inner.into_query_set().bind_cache(self.punnu.clone());
        qs.fetch_all(ctx)
    }

    /// Run the queryset, return exactly one matching row, and mirror that
    /// row into the bound Punnu. Same error semantics as
    /// [`QuerySet::fetch_one`].
    pub fn fetch_one<'ctx>(
        self,
        ctx: &'ctx mut crate::DjogiContext,
    ) -> impl std::future::Future<Output = Result<T, crate::DjogiError>> + Send + 'ctx
    where
        T: crate::pg::decode::FromPgRow + Send + Unpin + 'ctx,
    {
        let qs = self.inner.into_query_set().bind_cache(self.punnu.clone());
        qs.fetch_one(ctx)
    }

    /// Run the queryset, return the first matching row (or `None`), and
    /// mirror that row — when present — into the bound Punnu.
    pub fn first<'ctx>(
        self,
        ctx: &'ctx mut crate::DjogiContext,
    ) -> impl std::future::Future<Output = Result<Option<T>, crate::DjogiError>> + Send + 'ctx
    where
        T: crate::pg::decode::FromPgRow + Send + Unpin + 'ctx,
    {
        let qs = self.inner.into_query_set().bind_cache(self.punnu.clone());
        qs.first(ctx)
    }

    /// Run the queryset and return the row count.
    /// Cache binding is intentionally not applied — `COUNT(*)` returns no
    /// rows for the identity map to absorb. Adopters wanting both a count
    /// and the rows themselves should issue separate `fetch_all` and
    /// `count` calls; the framework does not synthesise a count from a
    /// cached row vector because the count must reflect the database, not
    /// the locally cached subset.
    pub fn count<'ctx>(
        self,
        ctx: &'ctx mut crate::DjogiContext,
    ) -> impl std::future::Future<Output = Result<i64, crate::DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
    {
        self.inner.into_query_set().count(ctx)
    }
}

#[cfg(any(test, feature = "testing"))]
impl<T> CachedPortableQuerySet<'_, T>
where
    T: Model + crate::types::Cacheable + Clone + crate::pg::decode::FromPgRow,
{
    /// Render the plain `SELECT` SQL for test assertions.
    /// Mirrors [`QuerySet::render_select_sql_for_testing`] without forcing
    /// tests to unwrap the cache-bound wrapper.
    pub fn render_select_sql_for_testing(&self) -> Result<String, PortablePredicateError> {
        self.inner.as_query_set().render_select_sql_for_testing()
    }
}

impl<T: Model + crate::types::Cacheable + Clone> std::fmt::Debug for CachedPortableQuerySet<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.as_query_set().fmt(f)
    }
}

// Manual `Clone` for `QuerySet<T>`. `Q<T>` itself
// carries a manual `Clone` impl (no `T: Clone` propagation), so this
// just delegates `condition.clone` directly without lowering through
// `q_to_condition_ref`. The pre-PR2b lower-and-rewrap approach forced
// every clone to flatten the trusted-portable wrapper into a legacy
// `Condition` tree, which `BasicPredicate::Field(_)` could not survive
// once the `Q::Portable` flip exposed Sassi `Field` leaves. Cloning the
// `Q<T>` directly preserves the trusted-provenance shape across clones.
impl<T: Model> Clone for QuerySet<T> {
    fn clone(&self) -> Self {
        QuerySet {
            condition: self.condition.clone(),
            ordering: self.ordering.clone(),
            has_explicit_ordering: self.has_explicit_ordering,
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
            // 3: `Arc::clone` on the trait-object handle
            // the underlying `Punnu<T>` is `Arc`-internal so the bind
            // semantics carry through every queryset clone. A clone of
            // a `.cache(&p)`-bound queryset still feeds the same `p`.
            cache_target: self.cache_target.clone(),
            _model: PhantomData,
        }
    }
}

// `Debug` impl uses the manual `Q<T>: Debug` walker
// directly. Pre-PR2b the impl lowered through `q_to_condition_ref` to
// avoid `T: Debug`; the new `Q<T>` manual `Debug` walker handles the
// model-bound elision in one place, so this impl just forwards. SQL
// structure visibility is preserved (the variant-walker prints
// `Q::Portable(_)` / `Q::Compound { op, parts }` etc. just like the
// legacy `Condition` shape did) without round-tripping through the
// retired bridge.
impl<T: Model> std::fmt::Debug for QuerySet<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 3: `cache_target` is intentionally excluded
        // from the Debug projection. The cache modifier is purely
        // additive on the post-fetch side (SQL output, query plan,
        // and every accumulator on the build path are byte-identical
        // whether `.cache(...)` was called or not), so the Debug
        // shape — which downstream tests grep against to check SQL
        // structure — must stay invariant under `.cache(...)`.
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

#[cfg(any(test, feature = "testing"))]
impl<T> QuerySet<T>
where
    T: Model + crate::pg::decode::FromPgRow,
{
    /// Render the plain `SELECT` SQL for test assertions.
    /// Exposed only under the `testing` feature so integration tests can pin
    /// SQL-emitter invariants without coupling to the `Debug` projection.
    pub fn render_select_sql_for_testing(&self) -> Result<String, PortablePredicateError> {
        crate::query::sql::build_select(self).map(|acc| acc.into_parts().0)
    }
}

impl<T: Model> Default for QuerySet<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// AND-combine an arbitrary `IntoQ<T>` term onto an existing `Q<T>`
/// substrate without flattening the existing structure.
/// Replaces the pre-PR2b
/// `and_condition_into_q(Q<T>, Condition) -> Q<T>` helper. The pre-PR2b
/// helper lowered the existing `Q<T>` through `q_to_condition` and
/// re-wrapped the combined result as `Q::Condition(_)`, which destroyed
/// the trusted-provenance shape of any inner `Q::Portable` leaves. The
/// new helper composes through `Q<T>: BitAnd<Q<T>>` so portable leaves
/// stay reducible at the cache boundary (PR4) and `Q::Portable & Q::Portable`
/// flattens through the trusted Sassi reducer.
/// `Q::Portable(PortablePredicate::True)` (the unfiltered identity)
/// short-circuits to the addition unchanged so unfiltered querysets do
/// not pick up a redundant `TRUE AND ...` wrapper.
fn and_q_into_q<T: Model, A: crate::query::IntoQ<T>>(current: Q<T>, addition: A) -> Q<T> {
    let added = addition.into_q();
    if is_q_vacuously_true(&current) {
        added
    } else {
        current & added
    }
}

/// Test whether a `Q<T>` is structurally vacuously TRUE. Mirrors the
/// `query::sql::q_is_vacuously_true` helper (kept module-private there);
/// duplicating the small walker here lets `and_q_into_q` short-circuit
/// without reaching across crate-private boundaries while the legacy
/// SQL path remains in transition.
fn is_q_vacuously_true<T: Model>(q: &Q<T>) -> bool {
    use crate::query::q::CompoundOp;
    match q {
        Q::Portable(p) => match p.inner_ref() {
            sassi::BasicPredicate::True => true,
            sassi::BasicPredicate::And(parts) => parts
                .iter()
                .all(|c| matches!(c, sassi::BasicPredicate::True)),
            _ => false,
        },
        Q::Condition(c) => c.is_vacuously_true(),
        Q::Compound {
            op: CompoundOp::And,
            parts,
        } => parts.iter().all(is_q_vacuously_true),
        Q::Negated(inner) => match inner.as_ref() {
            Q::Portable(p) => matches!(p.inner_ref(), sassi::BasicPredicate::False),
            Q::Compound {
                op: CompoundOp::Or,
                parts,
            } => parts.is_empty(),
            _ => false,
        },
        _ => false,
    }
}

impl<T: Model> QuerySet<T> {
    /// Construct an empty QuerySet — or, for proxy models, one seeded
    /// with the proxy's `#[model(default_filter, default_order)]` state.
    /// Prefer `T::objects` at call sites — it is the idiomatic
    /// spelling and reads as "all objects of this model (before
    /// filtering)".
    /// # Proxy default-filter / default-order seeding
    /// Reads [`Model::default_filter_condition`] and
    /// [`Model::default_order_by`] at construction time. Non-proxy
    /// models inherit the default trait impls (returns `None` /
    /// `Vec::new`) so the seeded queryset is structurally identical
    /// to the prior surface; rustc inlines the `None` / empty `Vec`
    /// returns and folds the seeding step away on the hot path.
    /// Proxy models override the trait methods via the macro, so the
    /// seeded queryset starts with the proxy's lowered SQL fragment
    /// already in `condition` and the proxy's ordering already in
    /// `ordering`. Subsequent `.filter(...)` calls AND-compose with
    /// the default (matching Django-style semantics — the proxy
    /// filter is the prefix no adopter call can drop), and `.order_by(...)`
    /// calls APPEND to the default ordering per the existing queryset
    /// convention (`queryset.rs` lines 25–28).
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn new() -> Self {
        // Proxy default filter → Q<T> (substrate flip).
        // `T::default_filter_condition` returns `Option<Condition>`; wrap
        // any returned condition in `Q::Condition(c)` so it round-trips
        // through the bridge with identical SQL. Non-proxy models return
        // `None` → `Q::always_true` (same vacuous-truth as the pre-flip
        // `Condition::True` default).
        let condition = T::default_filter_condition().map_or_else(Q::always_true, |c| {
            use crate::query::q::Q;
            Q::Condition(c)
        });
        let ordering = T::default_order_by();
        QuerySet {
            condition,
            ordering,
            has_explicit_ordering: false,
            distinct: DistinctMode::None,
            limit: None,
            offset: None,
            is_empty: false,
            prefetch_paths: Vec::new(),
            select_related_paths: Vec::new(),
            lock: crate::query::lock::LockMode::None,
            // 3: opt-in. The default queryset has no
            // cache binding; `.cache(&p)` (defined in a separate
            // `impl<T: Model + sassi::Cacheable>` block) sets this
            // to `Some(_)`.
            cache_target: None,
            _model: PhantomData,
        }
    }

    /// Performs an `INNER JOIN LATERAL` against the provided `inner` queryset.
    /// Returns a [`LateralQuerySet`] that evaluates to `(L, R)` tuples.
    /// Parent rows (`L`) are dropped from the result if the `inner`
    /// queryset returns no rows for that parent.
    /// Inside `inner`, you can use `OuterRef::as_lateral_outer_expr` to
    /// refer to columns from this outer queryset.
    pub fn join_lateral<R: Model>(
        self,
        inner: QuerySet<R>,
    ) -> crate::query::lateral::LateralQuerySet<T, R, crate::query::lateral::InnerLateral> {
        crate::query::lateral::LateralQuerySet {
            outer: self,
            inner,
            _mode: std::marker::PhantomData,
        }
    }

    /// Performs a `LEFT JOIN LATERAL` against the provided `inner` queryset.
    /// Returns a [`LateralQuerySet`] that evaluates to `(L, Option<R>)` tuples.
    /// Parent rows (`L`) are preserved with `None` if the `inner`
    /// queryset returns no rows for that parent.
    /// Inside `inner`, you can use `OuterRef::as_lateral_outer_expr` to
    /// refer to columns from this outer queryset.
    pub fn left_join_lateral<R: Model>(
        self,
        inner: QuerySet<R>,
    ) -> crate::query::lateral::LateralQuerySet<T, R, crate::query::lateral::LeftLateral> {
        crate::query::lateral::LateralQuerySet {
            outer: self,
            inner,
            _mode: std::marker::PhantomData,
        }
    }

    /// Structural empty QuerySet — every terminal method (Task 6) short-
    /// circuits to the empty result without touching the database.
    /// Takes `self` as an instance transform (matching Django's
    /// `queryset.none` ergonomics) so `Post::objects.none` compiles
    /// and reads naturally. Any filters / ordering / limits already
    /// accumulated on `self` are discarded — the returned queryset is a
    /// fresh [`QuerySet::new`] with `is_empty = true`. From-scratch
    /// construction is spelled `QuerySet::<T>::new.none`.
    /// Useful for authorization / feature-flag branches:
    /// ```ignore
    /// let qs = if user.is_authenticated {
    ///     Post::objects().filter(|f| f.published.eq(true))
    /// } else {
    ///     Post::objects().none()
    /// };
    /// ```
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn none(self) -> Self {
        // `self` is intentionally ignored — `.none` is a structural reset,
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
    /// `T::Fields` (a ZST) and returns any [`IntoQ`](crate::query::IntoQ)
    /// predicate.
    /// ```ignore
    /// Post::objects().filter(|f| f.published().eq(true))
    /// ```
    /// Ordinary generated root fields now return Djogi-owned portable
    /// predicates, so `f.col.eq(...)` / `icontains(...)` filters can pass the
    /// cache and refresh portability gates. PostgreSQL-specific predicates
    /// remain valid database filters through
    /// [`DjogiField::explicit_pg_predicate`](crate::query::field::DjogiField::explicit_pg_predicate)
    /// and are rejected by cache boundaries.
    /// Use [`filter_struct`](Self::filter_struct) for closure-free dynamic
    /// builders, macro-generated `{Model}Filter` values, or direct `Q<T>`
    /// composition outside a field closure. Legacy raw [`Condition`] values
    /// still lift into `Q::Condition(_)` for byte-for-byte SQL parity.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn filter<F, P>(mut self, f: F) -> Self
    where
        F: FnOnce(T::Fields) -> P,
        P: crate::query::IntoQ<T>,
    {
        let added = f(T::Fields::default());
        self.condition = and_q_into_q(self.condition, added);
        self
    }

    /// AND an expression-IR predicate onto the condition tree.
    /// The closure receives a default-constructed `T::Fields` handle
    /// and must return an [`Expr<bool>`](crate::expr::Expr) — i.e. a
    /// comparison produced by the `eq` / `neq` / `gt` / `gte` / `lt` /
    /// `lte` methods on `Expr<T>`. The returned expression is wrapped
    /// in [`Condition::Expr`] and AND-ed onto `self.condition`; the
    /// SQL emitter walks the expression via
    /// [`crate::expr::sql::emit_expr`] instead of the
    /// column-vs-literal leaf emitter.
    /// # When to reach for `filter_expr` over `filter`
    /// [`QuerySet::filter`] accepts predicates where the RHS
    /// is always a literal — `f.balance.lt(100i64)`. `filter_expr`
    /// generalises both sides: either operand can be a column ref, a
    /// literal, or an arithmetic expression. Use it for:
    /// - Field-vs-field comparisons (`balance < overdraft_limit`).
    /// - Arithmetic predicates (`balance + pending_credit > 0`).
    /// - Predicates that build on [`crate::expr::Expr`] composition
    ///   future tasks extend this surface with aggregates, subqueries,
    ///   and `CASE` (Tasks 4/5).
    ///   The two methods compose: a queryset may have any mix of
    ///   `filter` and `filter_expr` clauses, and every call is AND-ed
    ///   onto the same tree.
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
        // : `IntoQ<T> for Expr<bool>` lifts to
        // `Q::Expression(_)` so this method shares the same generalised
        // composition surface as `filter`. Kept as a distinct method
        // for readability — the type signature documents the intent
        // (this filter is an expression-IR boolean).
        self.condition = and_q_into_q(self.condition, expr);
        self
    }

    /// Add a typed filter closure **negated** (wrapped in SQL `NOT`), AND-ed
    /// onto the existing tree. Equivalent to Django's `QuerySet.exclude`.
    /// ```ignore
    /// Post::objects().exclude(|f| f.title().eq("draft".to_string()))
    /// ```
    /// See [`filter`](Self::filter) for the current portable-vs-SQL-only
    /// routing. Pure portable predicates are negated inside the trusted
    /// portable algebra so cache and refresh gates can still reduce them;
    /// SQL-only predicates are wrapped as `Q::Negated(...)`.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn exclude<F, P>(mut self, f: F) -> Self
    where
        F: FnOnce(T::Fields) -> P,
        P: crate::query::IntoQ<T>,
    {
        let added = f(T::Fields::default()).into_q();
        // `exclude_struct`-style negation dispatch (
        // PR2 Step 4): if the returned `Q<T>` is purely portable, push
        // the negation into the trusted `PortablePredicate<T>` (so
        // `!Portable(p)` rides Sassi's `Not` and double-negation
        // collapses in place). Otherwise wrap as `Q::Negated(...)`.
        // This preserves portability for portable predicates and keeps
        // the cache boundary's audit visibility intact.
        let negated = match added {
            Q::Portable(p) => Q::Portable(!p),
            other => !other,
        };
        self.condition = and_q_into_q(self.condition, negated);
        self
    }

    /// AND a [`Q<T>`] predicate onto the condition tree.
    /// Accepts any [`IntoQ<T>`] — `Q<T>` directly, a Djogi-owned
    /// [`PortablePredicate`](crate::query::PortablePredicate), an
    /// `Expr<bool>`, a legacy [`Condition`](crate::query::Condition), or a
    /// `{Model}Filter` programmatic builder (the macro emits an
    /// `IntoQ<#model>` impl alongside the existing
    /// [`ModelFilter`](crate::query::ModelFilter)). Raw
    /// `sassi::BasicPredicate<T>` is deliberately excluded: it can pair a
    /// forged SQL column name with an unrelated Rust extractor.
    /// Legacy `Condition` inputs still produce character-for-character SQL
    /// parity with the legacy `Condition` substrate. `{Model}Filter`
    /// inputs keep `FilterClause` as their single source of truth and lazily
    /// reconstruct portable Q leaves for conservative bool/string equality and
    /// membership clauses; unsupported fields, wrapped/optional shapes, value
    /// mismatches, and SQL-only operators fall back to `Q::Condition`.
    /// Empty `{Model}Filter` bodies short-circuit — no AND-ing, no vacuous
    /// `TRUE` sub-tree. Single portable clauses remain a single Q leaf; single
    /// fallback clauses remain a plain `Condition::Leaf` inside
    /// `Q::Condition(_)`, so SQL emission avoids redundant parentheses.
    /// This is the closure-free sibling of [`QuerySet::filter`]. Use this
    /// method from shell bindings, admin UIs, any dynamic assembler that can't
    /// write a `|f|` closure at compile time, and any new caller composing a
    /// `Q<T>` directly through the public algebra.
    /// ```ignore
    /// // ModelFilter — closure-free
    /// let filter = PostFilter::new()
    ///     .published(Lookup::Eq(true))
    ///     .view_count(Lookup::Gte(50i32));
    /// let rows = Post::objects().filter_struct(filter).fetch_all(&pool).await?;
    ///
    /// // Q<T> — public algebra
    /// let q: Q<Post> = Q::Ilike(Post::fields().title, "rust%".into());
    /// let rows = Post::objects().filter_struct(q).fetch_all(&pool).await?;
    /// ```
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn filter_struct<F: crate::query::IntoQ<T>>(mut self, filter: F) -> Self {
        // : AND the incoming `Q<T>` directly without
        // round-tripping through `q_to_condition`. The vacuous-truth
        // short-circuit fires before `Q<T>::bitand` so an empty filter
        // (e.g. an unfiltered `{Model}Filter` body) does not pick up a
        // synthetic `TRUE AND ...` wrapper at the SQL emit time.
        let q = filter.into_q();
        if is_q_vacuously_true(&q) {
            return self;
        }
        self.condition = and_q_into_q(self.condition, q);
        self
    }

    /// AND the **negation** of a [`Q<T>`] predicate onto the condition
    /// tree. The struct-API counterpart of [`QuerySet::exclude`]
    /// the closure-free version of `.exclude(|f| ...)`.
    /// Pure portable predicates are negated inside
    /// [`PortablePredicate`](crate::query::PortablePredicate), so Sassi can
    /// preserve Rust-side evaluability and simplify identities (`NOT TRUE`
    /// becomes false, `NOT FALSE` becomes true). Non-portable predicates are
    /// wrapped in `Q::Negated` and render as SQL `NOT (...)`.
    /// Unlike [`QuerySet::filter_struct`], an empty filter is **not**
    /// short-circuited — `NOT TRUE` is `FALSE`, and silently dropping it would
    /// produce a different result set.
    /// Sister method to [`QuerySet::filter_struct`]; the two compose
    /// freely:
    /// ```ignore
    /// Post::objects()
    ///     .filter_struct(PostFilter::new().published(Lookup::Eq(true)))
    ///     .exclude_struct(PostFilter::new().title(Lookup::Eq("draft".to_string())))
    /// ```
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn exclude_struct<F: crate::query::IntoQ<T>>(mut self, filter: F) -> Self {
        let q = filter.into_q();
        // Negation dispatch:
        // - Pure-portable `Q::Portable(p)`: push the negation into the
        // trusted predicate so the stored shape stays
        // `Q::Portable(!p)`. Sassi's `Not` reducer collapses double
        // negation in place, which keeps cache-boundary portability
        // gates from producing `Q::Negated(Q::Portable(_))` shells.
        // - Anything else: wrap as `Q::Negated(Box<Q<T>>)`. The SQL
        // emitter renders this as `NOT (...)`; cache-boundary
        // portability checks (PR4) can distinguish the two negations.
        // No vacuous-truth short-circuit — `NOT TRUE` is `FALSE`, and
        // silently dropping it would change the result set.
        let negated = match q {
            Q::Portable(p) => Q::Portable(!p),
            other => !other,
        };
        self.condition = and_q_into_q(self.condition, negated);
        self
    }

    /// Append one or more ordering expressions. Later `order_by` calls
    /// **append** to the existing ordering rather than replacing it, matching
    /// Django semantics: library code can add a stable tiebreaker without
    /// clobbering the caller's primary ordering.
    /// The closure can return either a single `OrderExpr` or a
    /// `Vec<OrderExpr>` — the `Into<Vec<OrderExpr>>` bound bridges both.
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
        self.has_explicit_ordering = true;
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

    /// Reject SELECT-only read-tail modifiers on mutating terminals.
    /// Bulk UPDATE / DELETE currently emit only `WHERE` state; allowing
    /// `limit`, `offset`, `distinct`, row locks, explicit `order_by`,
    /// `prefetch`, `select_related`, or `cache_target` would silently
    /// ignore caller intent.
    pub(crate) fn validate_mutation_read_tail(
        &self,
        terminal: &str,
    ) -> Result<(), crate::DjogiError> {
        let mut rejected: Vec<&'static str> = Vec::new();
        if self.limit.is_some() {
            rejected.push("limit");
        }
        if self.offset.is_some() {
            rejected.push("offset");
        }
        if !matches!(self.distinct, DistinctMode::None) {
            rejected.push("distinct");
        }
        if self.lock != crate::query::lock::LockMode::None {
            rejected.push("row locks");
        }
        if self.has_explicit_ordering {
            rejected.push("order_by");
        }
        if !self.prefetch_paths.is_empty() {
            rejected.push("prefetch");
        }
        if !self.select_related_paths.is_empty() {
            rejected.push("select_related");
        }
        if self.cache_target.is_some() {
            rejected.push("cache_target");
        }
        if rejected.is_empty() {
            return Ok(());
        }
        let rejected = rejected.join(", ");
        Err(crate::DjogiError::Validation(format!(
            "{terminal}() does not support queryset read-tail modifiers ({rejected}); remove them before calling {terminal}()"
        )))
    }

    /// Apply SQL `LIMIT n`. Replaces any prior `limit` value.
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
    /// # Footgun — wrap in `atomic`
    /// A `FOR UPDATE` lock is scoped to the active transaction. A
    /// pool-backed context auto-commits each statement, so
    /// `Post::objects.select_for_update.fetch_all(&mut pool_ctx)`
    /// acquires the lock and releases it the instant the implicit
    /// transaction closes — **no mutual exclusion** against a concurrent
    /// writer between the fetch and the subsequent `save`. Every
    /// correctness-sensitive use of `select_for_update` MUST sit inside
    /// an [`atomic`](crate::transaction::atomic) scope.
    /// # Chaining with `nowait` / `skip_locked`
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
    /// The idiomatic shape for work-queue consumers: multiple workers
    /// can pull jobs concurrently without blocking each other.
    /// Callable standalone (implies `select_for_update`). Combining
    /// with [`nowait`](QuerySet::nowait) — last call wins.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn skip_locked(mut self) -> Self {
        self.lock = crate::query::lock::LockMode::ForUpdateSkipLocked;
        self
    }

    /// Append `FOR SHARE` to the emitted SELECT — acquire a shared
    /// (non-exclusive) row-level lock on every selected row for the
    /// duration of the enclosing transaction. Multiple concurrent
    /// readers may take the same `FOR SHARE` lock; writers (`UPDATE` /
    /// `DELETE`) block until the lock is released.
    /// Use this when several sessions need to read-and-decide against
    /// the same row without any of them intending to write
    /// (e.g., two services verifying an account balance before routing
    /// a transfer). For read-then-write inside the same transaction,
    /// reach for [`select_for_update`](QuerySet::select_for_update)
    /// instead — `FOR SHARE` does not protect against a peer reader
    /// upgrading to a writer.
    /// # Footgun — wrap in `atomic`
    /// A `FOR SHARE` lock is scoped to the active transaction. A
    /// pool-backed context auto-commits each statement, so
    /// `Account::objects.select_for_share.fetch_one(&mut pool_ctx)`
    /// acquires the lock and releases it the instant the implicit
    /// transaction closes — **no mutual exclusion** against a
    /// concurrent writer between the fetch and the subsequent decide
    /// step. Every correctness-sensitive use of `select_for_share`
    /// MUST sit inside an [`atomic`](crate::transaction::atomic)
    /// scope.
    /// # Chaining with contention variants
    /// `select_for_share` only sets the base FOR SHARE mode. To pick
    /// up `NOWAIT` / `SKIP LOCKED` semantics, call the dedicated
    /// FOR SHARE methods [`for_share_nowait`](QuerySet::for_share_nowait)
    /// or [`for_share_skip_locked`](QuerySet::for_share_skip_locked)
    /// directly. The historical [`nowait`](QuerySet::nowait) /
    /// [`skip_locked`](QuerySet::skip_locked) modifiers
    /// **unconditionally promote to the FOR UPDATE family** for
    /// backward compatibility — chaining them after `select_for_share`
    /// silently swaps the base lock back to FOR UPDATE, which is a
    /// footgun. Use the FOR SHARE-named modifiers when the base lock
    /// is FOR SHARE.
    /// .
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn select_for_share(mut self) -> Self {
        self.lock = crate::query::lock::LockMode::ForShare;
        self
    }

    /// Set the SELECT lock to `FOR SHARE NOWAIT` — acquire a shared
    /// (non-exclusive) row lock if available, else return immediately
    /// with Postgres SQLSTATE `55P03` (`lock_not_available`), which
    /// terminals classify as
    /// [`DjogiError::LockConflict`](crate::DjogiError::LockConflict).
    /// Callable standalone (implies `select_for_share`). Combining
    /// with [`for_share_skip_locked`](QuerySet::for_share_skip_locked)
    /// last call wins.
    /// See [`select_for_share`](QuerySet::select_for_share) for the
    /// pool-backed footgun and the rationale for keeping the FOR
    /// SHARE and FOR UPDATE contention modifiers on separate methods.
    /// .
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn for_share_nowait(mut self) -> Self {
        self.lock = crate::query::lock::LockMode::ForShareNowait;
        self
    }

    /// Set the SELECT lock to `FOR SHARE SKIP LOCKED` — silently skip
    /// rows currently locked exclusively by another session and return
    /// only the unlocked rows under a shared lock.
    /// The FOR SHARE analogue of
    /// [`skip_locked`](QuerySet::skip_locked): useful for multi-reader
    /// scans that should make progress on the unlocked subset without
    /// blocking on rows another session is mid-write on.
    /// Callable standalone (implies `select_for_share`). Combining
    /// with [`for_share_nowait`](QuerySet::for_share_nowait) — last
    /// call wins.
    /// See [`select_for_share`](QuerySet::select_for_share) for the
    /// pool-backed footgun. .
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn for_share_skip_locked(mut self) -> Self {
        self.lock = crate::query::lock::LockMode::ForShareSkipLocked;
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
    /// Calling `.prefetch(path)` twice with the same path is idempotent
    /// the second registration is a no-op by `source_column` equality.
    /// This matches the natural expectation ("I asked for the same
    /// relation twice; please don't run two queries") and makes
    /// composition-site chaining (library code + caller both registering
    /// the same prefetch) free of surprises.
    /// # Why the bounds split across `Source::Pk` and `Target::Pk`
    /// Prefetch stitches via a `LEFT JOIN` keyed on `Source::Pk`; the
    /// `IN (...)` bind needs `Source::Pk: Encode + Type`, and the
    /// HashMap that routes targets back to parents needs it `Eq + Hash
    /// + Clone`. `Target::Pk` is not used for filtering — only for the
    /// NULL-probe in the stitching query, which goes through the raw-
    /// value path and does not require any `Target::Pk` bounds beyond
    /// what [`Model`] already guarantees.
    /// `Target` itself picks up `FromRow + Clone + Unpin` so the loader
    /// can decode the `t.*` columns and mint a per-parent owned copy;
    /// see the header comment on
    /// [`crate::relation::prefetch`] for the rationale behind the Clone
    /// bound (not on `Model` itself, just on the prefetch path).
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
    /// Calling `.select_related(path)` twice with the same path is
    /// idempotent — the second registration is a no-op by
    /// [`RelationPath::source_column`] equality, matching the
    /// dedup rule on `prefetch`. No `Vec` spam, no duplicate join in
    /// the emitted SQL.
    /// # Why the bounds on `Child`
    /// The `select_related` emitter aliases every child column in the
    /// `SELECT` list under a `rel_{source_column}.{col}` prefix. The
    /// [`FromJoinedPgRow`](crate::pg::decode::FromJoinedPgRow) bound on
    /// `Child` captures the decoder that reads those aliased columns
    /// back into a concrete `Child` instance — the macro-emitted
    /// sibling of `FromRow` that takes a prefix parameter. Without
    /// this bound the emitter would have no way to decode the
    /// child side of the join.
    /// `Child: Send + Sync + 'static` is the erasure contract — the
    /// decoded child is boxed as `Box<dyn Any + Send + Sync>` so it can
    /// share storage with heterogeneous child types on a single
    /// queryset (`.select_related(owner).select_related(fuel_type)`).
    /// # Multi-relation per queryset
    /// Multiple `.select_related(...)` calls on the same queryset stack
    /// each produces its own `LEFT JOIN` with a `rel_{source_column}`
    /// alias. Aliases never collide because source columns are unique
    /// per parent model by construction. Multi-**hop** `select_related`
    /// (chained targets) is not supported: [`RelationPath`] only
    /// carries a single hop at the type level.
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
    /// `pub(crate)` because it is an implementation detail of the terminal
    /// methods, not user-facing API; users who need "does this queryset
    /// actually match rows?" should call `.exists`, which also runs
    /// the real SQL.
    pub(crate) fn is_empty(&self) -> bool {
        self.is_empty
    }

    /// Group this queryset by one or more key columns, transitioning into
    /// [`crate::query::grouped::GroupedQuerySet<T, K>`].
    /// The closure receives a default-constructed `T::Fields` handle and must
    /// return one `FieldRef<T, V>` (arity 1) or a tuple of `FieldRef`s (arity
    /// 2..=4) — any type that implements
    /// [`crate::query::grouped::IntoGroupKeyTuple`].
    /// `GroupedQuerySet` has no terminals. Call `.annotate(...)` on the result
    /// to attach aggregate expressions and enter the terminal-bearing
    /// [`crate::query::grouped::GroupedAnnotatedQuerySet`] state. Premature
    /// `.fetch_all` on `GroupedQuerySet` is a compile error.
    /// # Example
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
    /// This is a convenience entry point equivalent to
    /// `.group_by(f)` followed by setting the grouping mode to
    /// `GroupingMode::Rollup`. Call `.annotate(...)` on the result to
    /// attach aggregate expressions.
    /// # Example
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
    /// This is a convenience entry point equivalent to
    /// `.group_by(f)` followed by setting the grouping mode to
    /// `GroupingMode::Cube`. Call `.annotate(...)` on the result to
    /// attach aggregate expressions.
    /// # Example
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
    /// The key type is `` — there are no statically-typed key columns to
    /// decode because each row's "key" depends on which grouping set matched.
    /// Call `.annotate(...)` on the result to attach aggregate expressions;
    /// grouping-set column values are accessible via raw row access on the
    /// rows returned by `.fetch_all`.
    /// Arity-1 per set (one column per set). Multi-column sets are a
    /// future extension.
    /// # Example
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

    /// `GROUP BY GROUPING SETS (...)` — explicit multi-column sets,
    /// one tuple of columns per set.
    /// Accepts a closure that returns `Vec<Vec<&'static str>>`
    /// outer Vec is the list of sets; inner Vec is the list of
    /// columns in each set. An empty inner Vec is the "grand total"
    /// set (no GROUP BY columns; one row aggregating all input).
    /// Adopters extract column names from `T::Fields` accessors via
    /// each `FieldRef`'s `.column` method:
    /// ```ignore
    /// // Equivalent SQL:
    /// //   GROUP BY GROUPING SETS ((region, dept), (region), ())
    /// // Each result row is grouped by exactly one of the listed
    /// // tuples; the empty tuple yields the grand-total row.
    /// let rows = Sales::objects()
    ///     .grouping_sets(|f| vec![
    ///         vec![f.region().column(), f.dept().column()],
    ///         vec![f.region().column()],
    ///         vec![],
    ///     ])
    ///     .annotate(|f| f.amount().sum())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    /// Use [`Self::group_by_sets`] for the simpler arity-1-per-set
    /// shape (one column per set, no nested tuples). Use
    /// [`Self::rollup`] / [`Self::cube`] for hierarchical subtotal
    /// patterns.
    /// # Detecting subtotal rows
    /// Pair with [`crate::query::field::FieldRef::grouping`]
    /// inside `.annotate(...)` to flag which dimensions were rolled
    /// up in each result row:
    /// ```ignore
    /// .annotate(|f| (
    ///     f.amount().sum(),
    ///     f.region().grouping(),    // 1 if region rolled up, else 0
    ///     f.dept().grouping(),
    /// ))
    /// ```
    /// # Why `Vec<Vec<...>>` rather than typed tuple-of-tuples
    /// A typed signature like `qs.grouping_sets((set1), (set2), ...)`
    /// would need a `IntoGroupingSets` trait implemented for tuples
    /// of varying inner arity — not expressible in stable Rust without
    /// macros. The runtime `Vec<Vec<&'static str>>` shape preserves
    /// flexibility (sets of differing arities mixed freely) at the
    /// cost of one `vec![...]` allocation per call site. For the
    /// typical analytics-dashboard adopter who builds the sets once
    /// and runs the query repeatedly, the allocation is trivial.
    #[must_use = "grouped queries are lazy — dropping one silently omits the query"]
    pub fn grouping_sets<F>(self, f: F) -> crate::query::grouped::GroupedQuerySet<T, ()>
    where
        F: FnOnce(T::Fields) -> Vec<Vec<&'static str>>,
    {
        let sets = f(T::Fields::default());
        crate::query::grouped::GroupedQuerySet {
            qs: self,
            keys: (),
            grouping: crate::query::grouped::GroupingMode::Sets(sets),
            #[cfg(feature = "spatial")]
            spatial_source: None,
            _k: std::marker::PhantomData,
        }
    }

    // ── Tree-recursive transitions ───────────
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
    /// Works for **any** model `T` with at least one self-FK edge,
    /// without requiring `#[model(tree_edge = "...")]`. The caller
    /// supplies a typed [`RelationPath<T, T>`] picked from the
    /// macro-emitted `{T}Related::<edge>` accessor — the type-level
    /// pinning means a `RelationPath<Vehicle, Vehicle>` cannot be
    /// passed where the queryset is over `Post`.
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
    /// Emits:
    /// ```sql
    /// SELECT r.<pk-col> AS rk0, <aggregates>
    /// FROM <t-table> AS t
    /// LEFT JOIN <r-table> AS r ON ST_Contains(r.<r-geo-col>, t.<t-geo-col>)
    /// GROUP BY r.<pk-col>
    /// ```
    /// The `LEFT JOIN` gives users the unassigned bucket
    /// `RegionKey { region_pk: None }` — so rows that fall outside all known
    /// regions are visible in the result, not silently dropped (which an
    /// `INNER JOIN` would do).
    /// ## Runtime warning
    /// If `R` has no GiST index on its geography column, this method warns
    /// once per process via `tracing::warn!`. A spatial JOIN without a GiST
    /// index performs a full table scan on `R` for every row in `T`, scaling
    /// as O(|T| × |R|). Add `#[model(index = ...)]` on the region model's
    /// geography field or declare an `IndexSpec` with `IndexType::Gist`.
    /// ## Type parameters
    /// - `F` — closure that picks the geography column on `T`.
    /// - `G` — the concrete geography type (e.g. `GeoPoint`, `Polygon`).
    /// - `R` — the region model. Must have at least one `Geography`-typed field
    ///   in its descriptor.
    /// ## Panics
    /// Panics at call time if `R`'s descriptor contains no
    /// `FieldSqlType::Geography` field. This is a programming error (missing
    /// geo column on the region model), not a runtime condition, so a panic is
    /// appropriate — the same way an out-of-bounds slice index is.
    #[cfg(feature = "spatial")]
    #[must_use = "grouped queries are lazy — dropping one silently omits the query"]
    pub fn group_by_region<F, G, R, S>(
        self,
        field: F,
        _regions: QuerySet<R>,
    ) -> crate::query::grouped::GroupedQuerySet<T, crate::query::spatial_grouping::RegionKey<R>>
    where
        F: FnOnce(T::Fields) -> S,
        S: crate::query::field::IntoSqlField<T, G>,
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
        // PR3: `IntoSqlField` accepts both legacy `FieldRef<T, G>` and the
        // post-flip root accessor return type `DjogiField<T, G>`. The trait
        // is sealed so downstream code cannot smuggle hand-rolled column
        // strings; both implementers forward the validated column metadata
        // produced by `__make_field_ref`. Spatial group keys are not
        // predicate boundaries — adopters keep `f.location` direct
        // without an `.explicit_pg_predicate` step.
        let t_geo_col = field(T::Fields::default()).into_sql_field().column();

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

    /// Sugar for `group_by_region(..).annotate(|_| id_field.count_star)`.
    /// Returns a `GroupedAnnotatedQuerySet` that counts rows per region
    /// (including an unassigned bucket for rows outside all regions).
    /// The count aggregate is `COUNT(*)` on the data table alias `t`.
    /// Callers who need a different aggregate (e.g. `SUM(amount)`) use
    /// `group_by_region` directly and call `.annotate` themselves.
    /// ## Type parameters
    /// Same bounds as [`QuerySet::group_by_region`].
    #[cfg(feature = "spatial")]
    #[must_use = "grouped queries are lazy — dropping one silently omits the query"]
    pub fn count_by_region<F, G, R, S>(
        self,
        field: F,
        regions: QuerySet<R>,
    ) -> crate::query::grouped::GroupedAnnotatedQuerySet<
        T,
        crate::query::spatial_grouping::RegionKey<R>,
        crate::expr::AggregateExpr<i64>,
    >
    where
        F: FnOnce(T::Fields) -> S,
        S: crate::query::field::IntoSqlField<T, G>,
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
    /// Emits:
    /// ```sql
    /// SELECT ST_ClusterDBSCAN(t.<col>::geometry, $eps, $minpoints) OVER () AS cluster_id,
    ///        <aggregates>
    /// FROM <table> AS t
    /// [WHERE ...]
    /// GROUP BY cluster_id
    /// ```
    /// `cluster_id = NULL` for noise points (isolated rows with fewer than
    /// `minpoints` neighbours within `eps`). With the default `min_points = 1`
    /// every row is a core point of its own cluster, so noise never appears.
    /// # Filter ordering
    /// SQL evaluates `WHERE` before window functions, so `.filter(...)` calls
    /// chained *before* `cluster_by_proximity` prune points **from the DBSCAN
    /// input** — the clustering sees only the survivors, which can produce
    /// different cluster ids than clustering the full set and filtering
    /// after. Use `.having(...)` (evaluated after `GROUP BY cluster_id`) when
    /// you want to filter on aggregate output without changing which rows
    /// participate in the clustering.
    /// # Example
    /// ```ignore
    /// let counts = Store::objects()
    ///     .cluster_by_proximity(|f| f.location(), ClusterRadius::meters(500.0).min_points(3))
    ///     .annotate(|f| f.id.count_star())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    /// # Type parameters
    /// - `F` — closure that resolves the geography column from `T::Fields`.
    /// - `G` — concrete geography type (e.g. `GeoPoint`, `Polygon`).
    #[cfg(feature = "spatial")]
    #[must_use = "grouped queries are lazy — dropping one silently omits the query"]
    pub fn cluster_by_proximity<F, G, S>(
        self,
        field: F,
        radius: crate::query::spatial_grouping::ClusterRadius,
    ) -> crate::query::grouped::GroupedQuerySet<T, crate::query::spatial_grouping::ClusterId>
    where
        F: FnOnce(T::Fields) -> S,
        S: crate::query::field::IntoSqlField<T, G>,
        G: crate::geo::GeographyValue,
    {
        // PR3: accept legacy `FieldRef<T, G>` and the post-flip root
        // accessor return type `DjogiField<T, G>`. Spatial cluster keys
        // are not predicate boundaries — `f.location` stays direct
        // without an `.explicit_pg_predicate` step.
        let t_geo_col = field(T::Fields::default()).into_sql_field().column();
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
    /// Emits:
    /// ```sql
    /// SELECT ST_GeoHash(t.<col>::geometry, $precision) AS geohash, <aggregates>
    /// FROM <table> AS t
    /// [WHERE ...]
    /// GROUP BY geohash
    /// ```
    /// Geohash strings are prefix-ordered: a `P5` key is a prefix of any
    /// `P6` key that falls in the same parent cell — coarser re-aggregation
    /// is possible via string truncation without re-querying. Nullable
    /// geography columns (`Option<G>`) bucket NULLs into `GeohashKey(None)`;
    /// non-nullable columns never produce `None`.
    /// # Filter ordering
    /// SQL evaluates `WHERE` before the `ST_GeoHash` projection, so
    /// `.filter(...)` calls chained *before* `bucket_by_cell` prune points
    /// **from the bucketing input** — only the survivors are assigned a
    /// geohash and aggregated. Use `.having(...)` (evaluated after
    /// `GROUP BY geohash`) when you want to filter on aggregate output
    /// without affecting which rows are bucketed.
    /// # Example
    /// ```ignore
    /// let heatmap = Store::objects()
    ///     .bucket_by_cell(|f| f.location(), GeohashPrecision::P5)
    ///     .annotate(|f| f.id.count_star())
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    /// # Type parameters
    /// - `F` — closure that resolves the geography column from `T::Fields`.
    /// - `G` — concrete geography type (e.g. `GeoPoint`).
    #[cfg(feature = "spatial")]
    #[must_use = "grouped queries are lazy — dropping one silently omits the query"]
    pub fn bucket_by_cell<F, G, S>(
        self,
        field: F,
        precision: crate::query::spatial_grouping::GeohashPrecision,
    ) -> crate::query::grouped::GroupedQuerySet<T, crate::query::spatial_grouping::GeohashKey>
    where
        F: FnOnce(T::Fields) -> S,
        S: crate::query::field::IntoSqlField<T, G>,
        G: crate::geo::GeographyValue,
    {
        // PR3: accept legacy `FieldRef<T, G>` and the post-flip root
        // accessor return type `DjogiField<T, G>`. Geohash bucket keys
        // are not predicate boundaries — `f.location` stays direct
        // without an `.explicit_pg_predicate` step.
        let t_geo_col = field(T::Fields::default()).into_sql_field().column();
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

// Manual `.not_deleted` helper for `SoftDeletable`
// models.
// **Spec lock (line 971, RESOLVED 2026-05-03, lens, locked):**
// automatic default-filter composition is deferred to
// once the `Q<T>` substrate lands. Today only the manual helper
// ships — adopters must call `.not_deleted` explicitly on each
// `objects` chain that should exclude soft-deleted rows. A later
// change will replace this method with auto-composition under the
// new substrate.
// **Design notes:**
// 1. The bound is `M: crate::SoftDeletable` (re-exported through
// `crate::compose`). The trait already implies `M: Model` via its
// super-bound, so a separate `Model` bound is redundant.
// 2. The leaf is constructed by hand via [`Leaf::new`] (a
// crate-internal constructor) rather than going through
// [`FieldRef::is_null`]. Two reasons:
// - The macro-generated `T::Fields` ZST does not expose a
// `deleted_at` accessor on every model — only on those whose
// `#[model]` attribute injected the column. `SoftDeletable`-
// deriving models declare the column themselves (Path B), which
// means there's no compile-time guarantee that
// `T::Fields::default.deleted_at` exists at the type level.
// - The column name reads from `<M as SoftDeletable>::COLUMN`
// (defaults to `"deleted_at"` via the trait const).
// Reading via the trait surface lets a future column-override
// path (e.g. `#[model(soft_deletable(column = "trashed_at"))]`)
// flow through `.not_deleted` automatically — the helper is
// not a hard-coded literal anymore.
// 3. The `'static` bound on `M` mirrors the bounds present on the
// other terminal-method impls below (`fetch_all`, `count`, etc.)
// every `T::Fields::default` call site already requires it,
// so adding the same bound here keeps the impl block coherent
// when chained.
impl<M: crate::SoftDeletable + 'static> QuerySet<M> {
    /// Filter to rows where `deleted_at IS NULL` — the manual
    /// soft-delete exclusion helper.
    /// **Manual today; auto-composed under the future substrate.** This
    /// ships the manual helper only; adopters who want soft-deleted rows excluded
    /// must call `.not_deleted` on every `objects` chain. A future
    /// substrate flip will land automatic default-filter composition once the
    /// `Q<T>` substrate is in place — at which point this helper
    /// becomes redundant on the default code path. The method name
    /// will likely be retained as a no-op or as the explicit reverse
    /// of an `_insecurely` bypass; see spec line 971 for the
    /// migration plan.
    /// ```ignore
    /// // Soft-deletable model with the attribute on `#[model]`:
    /// #[model(table = "posts", soft_deletable)]
    /// pub struct Post {
    ///     pub title: String,
    ///     pub deleted_at: Option<djogi::DateTime>,
    /// }
    ///
    /// // Exclude trashed rows explicitly:
    /// let live = Post::objects()
    ///     .not_deleted()
    ///     .fetch_all(&mut ctx)
    ///     .await?;
    /// ```
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn not_deleted(mut self) -> Self {
        // Construct the `<column> IS NULL` leaf directly. `Leaf::new`
        // is `pub(crate)`, so this is the canonical in-crate path — no
        // need to route through the typed `FieldRef::is_null` (which
        // would require the column to be exposed on the macro-emitted
        // `T::Fields` ZST and would also pin the column type at
        // compile time, defeating the convention-by-name model the
        // `SoftDeletable` trait uses for its getter).
        // Read the column name through `<M as
        // SoftDeletable>::COLUMN` rather than a hard-coded `"deleted_at"`
        // string. The trait const defaults to `"deleted_at"` (canonical
        // case) but a future per-model rename can override the const
        // at the `impl` level — `.not_deleted` picks up the override
        // automatically without changing this call site.
        let leaf = crate::query::condition::Leaf::new(
            <M as crate::SoftDeletable>::COLUMN,
            crate::query::condition::LookupOp::IsNull,
            crate::query::condition::FilterValue::Null,
        );
        // Route through the new `and_q_into_q` helper.
        // `Condition` lifts to `Q::Condition(_)` via the sealed `IntoQ`
        // impl, preserving the legacy SQL emission shape this caller
        // depends on.
        self.condition = and_q_into_q(self.condition, Condition::Leaf(leaf));
        self
    }
}

// ── `.cache(&punnu)` opt-in modifier ─────────────────────
// Bound `T: Model + sassi::Cacheable` is split into a dedicated impl
// block for the same reason `not_deleted` lives in its own block: the
// extra trait bound is opt-in. Models that don't go through
// `#[derive(Model)]` (and therefore don't pick up the auto-emitted
// `Cacheable` impl) keep the existing `impl<T: Model> QuerySet<T>`
// surface unchanged — `.cache(...)` simply doesn't compile for them,
// matching the spec contract that the cache modifier is opt-in.
// Why `crate::types::Cacheable` (not `sassi::Cacheable`) for the
// bound: per `feedback_macro_path_routing.md`, code that names
// trait paths routes through `crate::types` so a future sassi
// reshuffle (e.g., moving `Cacheable` to a sub-module) only has to
// update one re-export instead of every `use sassi::Cacheable;`
// site in the framework. The trait identity is the same — the
// re-export at `djogi/src/types.rs` is `pub use sassi::cacheable::Cacheable;`
// so the bound resolves byte-identically.
// Spec anchors: §664 (`.cache(&punnu)` modifier; opt-in).
impl<T: Model + crate::types::Cacheable + Clone> QuerySet<T> {
    /// Internal helper: stamp this QuerySet with a type-erased Punnu cache
    /// target so the next terminal method sends each materialised row
    /// through [`sassi::Punnu::insert`] before returning to the caller.
    /// `pub(crate)` because the public adopter surface is [`QuerySet::cache`]
    /// , which gates entry through the trusted portable
    /// predicate reducer. `bind_cache` runs unconditionally with no gate, so
    /// only framework code that has already proven the queryset's predicate
    /// is portable (e.g. [`CachedPortableQuerySet`]'s terminal methods) may
    /// reach it.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub(crate) fn bind_cache(mut self, punnu: sassi::Punnu<T>) -> Self {
        // Wrap in the type-erased `CacheTarget` handle so the
        // queryset's struct field doesn't need a `T: Cacheable`
        // bound. `Arc::new` here boxes the wrapper once at bind
        // time; later clones of the queryset reuse the same `Arc`.
        // `Punnu::clone` itself is `Arc`-cheap, so the embedded
        // clone inside `PunnuCacheTarget::new` is also cheap.
        self.cache_target = Some(std::sync::Arc::new(PunnuCacheTarget::new(punnu)));
        self
    }

    /// Test-only mirror of [`bind_cache`](Self::bind_cache) — stamps
    /// a `cache_target` directly onto the QuerySet without going
    /// through the [`CachedPortableQuerySet`] terminal indirection.
    /// # Why exposed under `feature = "testing"`
    /// The public adopter path to a cache-bound queryset is
    /// `.cache(&punnu)`, which returns a [`CachedPortableQuerySet`]
    /// not a [`QuerySet`]. Adopters cannot, by design, construct a
    /// `QuerySet<T>` whose `cache_target` is `Some(_)` and then pass
    /// it to a set-op builder method like
    /// [`QuerySet::union`](crate::query::QuerySet::union). Defensive
    /// validation in
    /// [`crate::query::set_op::validate_arm`](crate::query::set_op)
    /// rejects such an arm if it is ever constructed (forward
    /// compatibility, in case a future API exposes
    /// `bind_cache`-shaped surface publicly).
    /// The #101) test suite needs a way to
    /// exercise that defensive branch from outside the crate. This
    /// helper is the supported test path: a `#[doc(hidden)]`,
    /// `feature = "testing"`-gated mirror that does exactly what
    /// `bind_cache` does. Adopters who do not opt into the `testing`
    /// feature never see it.
    /// # Not part of the stable API
    /// The `for_test` suffix and `#[doc(hidden)]` flag together
    /// communicate intent: this exists for in-tree fixture wiring, not
    /// adopter use. Production code should reach for the gated public
    /// path ([`cache`](Self::cache)) or, in this set-op context, build
    /// a cache-free arm before composing the set operation.
    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn bind_cache_for_test(self, punnu: sassi::Punnu<T>) -> Self {
        self.bind_cache(punnu)
    }

    /// Bind this QuerySet to a [`sassi::Punnu`] cache, gated by the
    /// trusted portable-predicate reducer.
    /// On success returns a [`CachedPortableQuerySet`] whose terminal methods
    /// (`fetch_all`, `fetch_one`, `first`, `count`) execute the underlying
    /// SQL and additionally feed every materialised row through
    /// [`sassi::Punnu::insert`] before handing the values back to the caller.
    /// Identity-map semantics + the configured [`sassi::OnConflict`] policy
    /// from sassi apply.
    /// On failure returns the original QuerySet alongside a typed
    /// [`PortablePredicateError`] — no terminal work or cache binding has
    /// taken place. SQL-only predicates (`Q::Condition`, `Q::Ilike`,
    /// `Q::JsonbPath`, `Q::Regex`, `Q::Expression`, `Q::Array`) are rejected;
    /// cache and refresh paths must be reducible to Sassi's Rust-evaluable
    /// algebra so cache reads can apply the same predicate in memory.
    /// [`QuerySet::none`] is treated as the portable false predicate and
    /// passes the gate even though its `is_empty` short-circuit means no
    /// row will ever be inserted.
    /// # Bounds
    /// `T: Clone` is required because the post-fetch hook runs after the
    /// terminal has materialised the `Vec<T>` for the caller — the cache
    /// target needs its own copy of each row to feed into `Punnu::insert`,
    /// while the caller still gets the original. Sassi's `Cacheable` does
    /// not include `Clone` as a supertrait, so the bound is added explicitly
    /// here. Every model that goes through `#[derive(Model)]` already has
    /// `#[derive(Clone)]` in the canonical recipe so this bound is satisfied
    /// by construction for every realistic adopter.
    /// # Why opt-in
    /// The cache hook is purely additive — SQL output, query plan, and the
    /// lifetime of the QuerySet are unchanged whether `.cache(...)` was
    /// called or not. Calling `.cache(&p)` records "this QuerySet feeds that
    /// Punnu instance"; not calling it preserves the uncached fetch
    /// behaviour exactly. Adopters who don't want a cache pay zero — neither
    /// in cycles nor in API surface.
    /// # Why `&Punnu<T>` (not `Punnu<T>`)
    /// `sassi::Punnu<T>` is `Arc`-internal — `Punnu::clone` clones a single
    /// `Arc<PunnuInner<T>>`. The builder takes the punnu by reference and
    /// clones internally so the call site reads as a binding ("feed THIS
    /// punnu") rather than a transfer ("hand over your punnu"). The cloned
    /// handle shares state with the caller's, matching the bind semantics
    /// the spec requires.
    /// # Errors propagation
    /// Per-row [`sassi::Punnu::insert`] errors (e.g.
    /// [`sassi::InsertError::Conflict`] under [`sassi::OnConflict::Reject`],
    /// or an L2-backend serialization failure) are logged via
    /// `tracing::warn!` and swallowed — they do not abort the fetch.
    /// Adopters who want to observe cache-side errors subscribe to
    /// [`sassi::Punnu::events`] (which fires per-insert events including
    /// conflict outcomes) — that is the designed observability surface for
    /// cache-pool lifecycle. Routing them through the fetch return type
    /// would conflate "Postgres said something went wrong" with "the cache
    /// mirror disagreed" and break the spec's contract that the cache
    /// modifier is purely additive.
    /// Predicate-portability errors travel through the `Result` return type,
    /// not through `tracing`, because they reflect a structural choice the
    /// adopter must repair (rewrite the filter using portable predicates)
    /// retrying the same queryset cannot turn a SQL-only predicate into a
    /// portable one.
    /// # Example
    /// ```ignore
    /// use djogi::cache::Punnu;
    /// use djogi::prelude::*;
    ///
    /// let pool: Punnu<Post> = Punnu::<Post>::builder().build();
    /// let recent = Post::objects()
    ///     .filter(|f| f.published().eq(true))
    ///     .order_by(|f| f.created_at().desc())
    ///     .limit(20)
    ///     .cache(&pool)?              // ← portable-gated cache binding
    ///     .fetch_all(&mut ctx)
    ///     .await?;
    /// // `pool.len() == recent.len()` — the 20 rows are now in
    /// // the bound Punnu's L1 identity map, ready for `pool.get(id)`.
    /// ```
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    // `clippy::result_large_err` is silenced because the `Err` variant returns
    // ownership of `self` to the caller alongside the typed portability error
    // the recovery shape (retry with a portable filter, or fall through to
    // ordinary SQL execution) requires the original queryset. Boxing would
    // force an allocation on every error path without changing what the
    // caller has to do with it; the established codebase idiom (see
    // `migrate/segment.rs`, `migrate/projection.rs`) keeps the allow at the
    // function boundary.
    #[allow(clippy::result_large_err)]
    pub fn cache(
        self,
        punnu: &sassi::Punnu<T>,
    ) -> Result<CachedPortableQuerySet<'_, T>, (Self, PortablePredicateError)> {
        match self.try_portable() {
            Ok(portable) => Ok(portable.cache(punnu)),
            Err((queryset, err)) => Err((queryset, err)),
        }
    }
}

/// Private marker trait used to seal [`IntoDistinctColumns`].
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
/// Expanding beyond six requires adding another
/// `impl_into_distinct_columns_tuple!` invocation below — the ceiling is
/// deliberately low because `DISTINCT ON` with more than a handful of
/// columns is a design smell, not a capacity limit Djogi cares to lift.
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

// PR3: `DjogiField<M, V>` is the post-flip root accessor return type.
// `DISTINCT ON` is a SQL-only emission boundary (no Punnu evaluator),
// so the wrapper forwards its column metadata through the same sealed
// trait. The bound stays sealed because both implementers route through
// the validated `__make_field_ref` / `__make_djogi_field` constructors
// downstream code cannot smuggle column strings through this interface.
impl<M: Model, V> distinct_seal::Sealed for crate::query::field::DjogiField<M, V> {}
impl<M: Model, V> IntoDistinctColumns for crate::query::field::DjogiField<M, V> {
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

/// Generate `IntoDistinctColumns` (plus the sealed marker) for a tuple
/// of `DjogiField`s. The tuple impls mirror the `FieldRef` set above so
/// post-PR3 root closures can return `(f.col_a, f.col_b, ...)`
/// directly without unwrapping each accessor through `__sql_field`.
/// Identifier safety stays sealed by the same mechanism — every
/// `DjogiField` carries a column string the validator already accepted.
macro_rules! impl_into_distinct_columns_djogi_tuple {
    ($($name:ident),+) => {
        impl<M: Model, $($name),+> distinct_seal::Sealed
            for ($(crate::query::field::DjogiField<M, $name>,)+)
        {}
        impl<M: Model, $($name),+> IntoDistinctColumns
            for ($(crate::query::field::DjogiField<M, $name>,)+)
        {
            fn into_distinct_columns(self) -> Vec<&'static str> {
                #[allow(non_snake_case)]
                let ($($name,)+) = self;
                vec![$($name.column()),+]
            }
        }
    };
}
impl_into_distinct_columns_djogi_tuple!(A);
impl_into_distinct_columns_djogi_tuple!(A, B);
impl_into_distinct_columns_djogi_tuple!(A, B, C);
impl_into_distinct_columns_djogi_tuple!(A, B, C, D);
impl_into_distinct_columns_djogi_tuple!(A, B, C, D, E);
impl_into_distinct_columns_djogi_tuple!(A, B, C, D, E, F);

// ── into_basic_predicate: conservative Q<T> → BasicPredicate<T> ─────
// Placed on `impl<T: Model> QuerySet<T>` (the base block) because extraction
// is purely structural — it walks the Q<T> condition tree without any
// DeltaSyncCacheable behaviour. The stub previously lived in the
// DeltaSyncCacheable-bounded block because it was a placeholder co-located
// with refresh_into. The real implementation now lives on the correct bound.
// Visibility: `pub` — adopters who want to inspect whether a QuerySet is
// reducible before calling refresh_into need to call this. It is not part of
// the everyday filter API (that is QuerySet::filter / filter_struct), but it
// is the intended entry point for advanced cache-integration code that needs
// to pass a BasicPredicate filter to sassi.
// Implementation note — Q::Condition is always Unreducible:
// Legacy SQL-only payloads and macro-generated `{Model}Filter` inputs route
// through Q::Condition(_) to preserve byte-for-byte SQL parity. A freshly
// constructed QuerySet<T>::new starts with Q::Portable(True). Ordinary
// closure filters whose field accessors return `PortablePredicate<T>` now
// preserve Q::Portable / Q::Compound / Q::Negated reducible forms, so
// cache and refresh gates accept them.
// Path-routing note (non-emitted code):
// Per `feedback_macro_path_routing.md`, path-routing governs macro-EMITTED
// code only. This impl block is non-emitted framework code; it may spell
// `sassi::BasicPredicate` directly.

fn cache_invalid(kind: &'static str) -> PortablePredicateError {
    PortablePredicateError::CacheInvalidNode { kind }
}

fn try_reduce_q_ref_to_basic<T: crate::model::Model>(
    q: &Q<T>,
) -> Result<sassi::BasicPredicate<T>, PortablePredicateError> {
    match q {
        Q::Portable(p) => Ok(p.clone().into_inner()),
        Q::Compound { op, parts } => {
            let mut reduced_parts = Vec::with_capacity(parts.len());
            for part in parts {
                reduced_parts.push(try_reduce_q_ref_to_basic(part)?);
            }
            #[allow(unreachable_patterns)]
            match op {
                CompoundOp::And => Ok(sassi::BasicPredicate::And(reduced_parts)),
                CompoundOp::Or => Ok(sassi::BasicPredicate::Or(reduced_parts)),
                _ => Err(cache_invalid("Compound::<unknown>")),
            }
        }
        Q::Xor(left, right) => Ok(sassi::BasicPredicate::Xor(
            Box::new(try_reduce_q_ref_to_basic(left)?),
            Box::new(try_reduce_q_ref_to_basic(right)?),
        )),
        Q::Negated(inner) => Ok(sassi::BasicPredicate::Not(Box::new(
            try_reduce_q_ref_to_basic(inner)?,
        ))),
        Q::Condition(_) => Err(cache_invalid("Condition")),
        Q::Ilike(_, _) => Err(cache_invalid("Ilike")),
        Q::Regex(_, _, _) => Err(cache_invalid("Regex")),
        Q::Expression(_) => Err(cache_invalid("Expression")),
        Q::Array(_) => Err(cache_invalid("Array")),
        Q::JsonbPath(_) => Err(cache_invalid("JsonbPath")),
        #[allow(unreachable_patterns)]
        _ => Err(cache_invalid("Q::<unknown>")),
    }
}

fn validate_portable_sql_emit<T: crate::model::Model>(
    predicate: &sassi::BasicPredicate<T>,
) -> Result<(), PortablePredicateError> {
    let mut acc = crate::pg::accumulator::SqlAccumulator::new("");
    // The predicate was produced by `try_reduce_q_ref_to_basic`, which
    // only descends `Q::Portable` / `Q::Compound` / `Q::Xor` / `Q::Negated`.
    // Every Field leaf therefore came out of a `PortablePredicate<T>`
    // (the trusted-construction boundary); JSON leaves transited the
    // MirJzSON builder. Pass `JsonTrust::Trusted` so the walker does
    // not reject Djogi-built JSON predicates during the gate.
    crate::query::portable::emit_basic_predicate::<T>(
        &mut acc,
        predicate,
        crate::query::SqlEmitContext::root(),
        crate::query::portable::JsonTrust::Trusted,
    )
}

impl<T: crate::model::Model> QuerySet<T> {
    /// Validate that this queryset is safe to use as a Punnu cache boundary.
    /// Ordinary SQL-only predicates remain valid for database execution, but
    /// cache and refresh paths must be reducible to Sassi's Rust-evaluable
    /// predicate algebra. `QuerySet::none` is treated as the portable
    /// false predicate rather than as an unfiltered query.
    pub fn is_portable(&self) -> Result<(), PortablePredicateError> {
        let portable_predicate = if self.is_empty {
            sassi::BasicPredicate::False
        } else {
            try_reduce_q_ref_to_basic(&self.condition)?
        };
        validate_portable_sql_emit::<T>(&portable_predicate)
    }

    /// Convert this queryset into the portable cache-boundary wrapper.
    /// On failure the original queryset is returned unchanged with the typed
    /// portability error; no terminal work or refresh task has started.
    // `clippy::result_large_err` allowed for the same reason as `cache`/
    // `refresh_into`: the `Err` variant returns ownership of `self` so the
    // caller can recover from a portability rejection. See the rationale on
    // [`QuerySet::cache`].
    #[allow(clippy::result_large_err)]
    pub fn try_portable(self) -> Result<PortableQuerySet<T>, (Self, PortablePredicateError)> {
        let portable_predicate = if self.is_empty {
            sassi::BasicPredicate::False
        } else {
            match try_reduce_q_ref_to_basic(&self.condition) {
                Ok(predicate) => predicate,
                Err(err) => return Err((self, err)),
            }
        };
        if let Err(err) = validate_portable_sql_emit::<T>(&portable_predicate) {
            return Err((self, err));
        }
        Ok(PortableQuerySet {
            inner: self,
            portable_predicate,
        })
    }

    /// Add a Djogi-trusted portable predicate closure to the query.
    /// This is the cache-safe sibling of [`QuerySet::filter`]: the closure
    /// must return a sealed [`IntoPortablePredicate`](crate::query::IntoPortablePredicate)
    /// value, so SQL-only `Condition`, `Expr<bool>`, and raw Sassi predicates
    /// cannot enter by accident.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn portable_filter<F, P>(mut self, f: F) -> Self
    where
        F: FnOnce(T::Fields) -> P,
        P: crate::query::IntoPortablePredicate<T>,
    {
        let predicate = f(T::Fields::default()).into_portable_predicate();
        self.condition = and_q_into_q(self.condition, predicate);
        self
    }

    /// Attempt to extract a [`sassi::BasicPredicate<T>`] from this QuerySet's
    /// filter tree.
    /// Returns `Some(predicate)` when the entire `Q<T>` condition tree is
    /// reducible to a `BasicPredicate<T>`. Returns `None` and emits a
    /// `tracing::warn!` when any node is unreducible.
    /// # Reducible variants
    /// | Q variant | Reduces to |
    /// |---|---|
    /// | `Q::Portable(p)` | `p.into_inner` — the inner [`sassi::BasicPredicate<T>`] is cloned out of the borrow so the walker does not require `T: Clone` |
    /// | `Q::Compound { And, all_reducible_parts }` | `BasicPredicate::And(parts)` |
    /// | `Q::Compound { Or, all_reducible_parts }` | `BasicPredicate::Or(parts)` |
    /// | `Q::Xor(left, right)` | `BasicPredicate::Xor(Box::new(reduced_left), Box::new(reduced_right))` |
    /// | `Q::Negated(reducible_inner)` | `BasicPredicate::Not(Box::new(reduced_inner))` — inner walked recursively, so `Q::Negated(Q::Compound{And, basics})` reduces too |
    /// | `QuerySet::none` (`is_empty == true`) | `BasicPredicate::False` |
    /// # Unreducible variants (always → `None`)
    /// `Q::Ilike`, `Q::JsonbPath`, `Q::Regex`, `Q::Expression`,
    /// `Q::Array`, `Q::Condition`.
    /// `Q::Condition` covers SQL-only payloads, including generated
    /// `{Model}Filter` clauses that fall outside the conservative portable
    /// mapping. Direct `Q<T>`, `PortablePredicate<T>`, ordinary generated
    /// field-accessor closure filters, and portable generated filter clauses
    /// stay reducible; a fresh `QuerySet::new` (no filters) starts as
    /// `Q::Portable(True)` and is reducible.
    /// # When to use this
    /// This is the inspection-style sibling of [`QuerySet::try_portable`].
    /// [`QuerySet::cache`] and [`QuerySet::refresh_into`]
    /// take the `Result`-returning route through `try_portable` so
    /// non-portable querysets are rejected with a typed error before any
    /// terminal work or refresh subscription begins. Reach for
    /// `into_basic_predicate` when you want to inspect a queryset's
    /// reducibility without consuming it through the cache/refresh boundary
    /// for example in tests or framework-internal cache-integration code
    /// that already knows it owns the queryset.
    /// # Visibility note
    /// This is `pub` for testability and to give framework-internal cache-
    /// integration code a named entry point. It is **not** an inspection
    /// API: the method consumes `self`, so callers cannot "inspect first,
    /// then refresh_into" — the QuerySet is moved by either call.
    /// `QuerySet::clone` preserves the reducible shape after Phase
    /// 8eta PR2b because `Q<T>` has a manual clone implementation that
    /// does not lower through `q_to_condition_ref`. A cloned portable
    /// queryset may therefore be inspected or refreshed without losing
    /// its trusted predicate structure.
    pub fn into_basic_predicate(self) -> Option<sassi::BasicPredicate<T>> {
        if self.is_empty {
            return Some(sassi::BasicPredicate::False);
        }
        match try_reduce_q_ref_to_basic(&self.condition) {
            Ok(predicate) => Some(predicate),
            Err(PortablePredicateError::CacheInvalidNode { kind }) => {
                tracing::warn!(
                    target: "djogi::cache",
                    model = std::any::type_name::<T>(),
                    reason = kind,
                    "QuerySet condition has non-portable predicates and cannot be \
                     reduced to a Sassi BasicPredicate. The cache and refresh paths \
                     reject non-portable querysets with a typed error; restructure \
                     the filter using only Djogi portable predicate operations to \
                     pass the cache/refresh boundary.",
                );
                None
            }
            Err(err) => {
                tracing::warn!(
                    target: "djogi::cache",
                    model = std::any::type_name::<T>(),
                    error = ?err,
                    "QuerySet condition could not be reduced to a portable cache predicate.",
                );
                None
            }
        }
    }
}

// ── 3 — delta-sync refresh subscription ────────────────────────
// `refresh_into` lives in its own impl block (separate from the base
// `impl<T: Model>` block and the `impl<T: Model + Cacheable + Clone>` cache
// block) because it requires the stricter combined bound
// `T: Model + DeltaSyncCacheable + Send + Sync + 'static`. Widening any
// existing block's bound would cascade to methods that have no need of
// `DeltaSyncCacheable`, breaking the clean opt-in layering.
// `into_basic_predicate` was previously stubbed here but has been
// moved to `impl<T: Model> QuerySet<T>` — extraction is purely
// structural (no DeltaSyncCacheable behaviour needed).
// Path-routing note (non-emitted code):
// Per `feedback_macro_path_routing.md`, path-routing governs macro-EMITTED
// code only. This impl block is non-emitted framework code; it may spell
// `sassi::Punnu`, `sassi::DeltaRefreshHandle`, `crate::auth::AuthContext`,
// `crate::pg::pool::DjogiPool`, and `sassi::BasicPredicate` directly.
impl<T> QuerySet<T>
where
    T: crate::model::Model
        + sassi::DeltaSyncCacheable
        + crate::pg::decode::FromPgRow
        + crate::cache::DjogiDeltaSyncMeta
        + Send
        + Sync
        + 'static,
    T::Watermark: tokio_postgres::types::ToSql + tokio_postgres::types::FromSqlOwned + Sync,
    T::Id: tokio_postgres::types::ToSql + Sync,
{
    /// Bind this QuerySet to a Punnu and start a delta-sync refresh subscription.
    /// On success, the fetcher owns a clone of the pool, the AuthContext by
    /// value, and the QuerySet's trusted BasicPredicate filter. SQL-only
    /// predicates return `Err((self, PortablePredicateError))` before any
    /// subscription starts. The fetcher NEVER captures `&mut DjogiContext`.
    /// # Real SQL path
    /// The fetcher's `fetch_delta` body now issues real SQL on every tick.
    /// Each tick acquires a fresh connection from the pool, constructs a
    /// `DjogiContext` with the captured `AuthContext` (auth-locked-to-
    /// subscription per spec §677), and runs
    /// `SELECT <columns> FROM <table> WHERE <watermark_col> >= $1
    /// [OR id IN ($2, …)] ORDER BY <watermark_col>` for delta ticks.
    /// Full baseline ticks with a portable filter push that filter into SQL.
    /// # Refresh knobs (spec §674)
    /// The returned `DeltaRefreshHandle<T>` exposes two adopter-facing knobs
    /// from sassi's native API — no djogi-side wrappers required:
    /// - **`with_eviction_recovery(bool)`** — when enabled, LRU evictions of
    ///   IDs this subscription has observed are passed to the fetcher as
    ///   `DeltaQuery::recover_ids` on a later delta tick. Opt in via
    ///   `handle.with_eviction_recovery(true)`.
    /// - **`with_periodic_full_refresh(Option<NonZeroUsize>)`** — schedule
    ///   full (non-delta) refreshes every N ticks. `Some(n)` makes every nth
    ///   scheduled tick use `since = None`. Opt in via
    ///   `handle.with_periodic_full_refresh(NonZeroUsize::new(10))`.
    ///   Both methods return `Self` for chaining:
    /// ```text
    /// let handle = MyModel::objects()
    ///     .refresh_into(&punnu, pool, auth)?
    ///     .with_eviction_recovery(true)
    ///     .with_periodic_full_refresh(NonZeroUsize::new(10));
    /// ```
    /// Additionally, the fetcher always monitors the Punnu event stream for
    /// LRU eviction events and emits a one-shot `tracing::warn!` on
    /// `djogi::cache` the first time an eviction is observed (spec §674
    /// Knob 1 — always-on, no adopter opt-in required).
    /// # Portable filter gate and full-baseline pushdown
    /// `refresh_into` first runs `try_portable`. Reducible predicates are
    /// carried as `BasicPredicate<T>`; SQL-only `Q<T>` arms are rejected. The
    /// fetcher pushes non-trivial portable filters into SQL only on full
    /// baseline ticks (`since == None` and no eviction-recovery ids). Delta
    /// ticks ignore the filter at SQL time so changed rows that transitioned
    /// out of the predicate can still be upserted or tombstoned correctly.
    /// `QuerySet::none` is preserved as a structural empty subscription:
    /// update ticks return empty deltas without querying the source table.
    /// # Interval placeholder
    /// The 30 s interval is a placeholder. A future change may add a builder for
    /// caller-supplied interval; see spec §672 review.
    // `clippy::result_large_err` is silenced for the same reason as
    // [`QuerySet::cache`]: the `Err` variant carries the original queryset
    // back to the caller for recovery. Boxing here would force allocations
    // on every rejection without changing the recovery shape.
    #[allow(clippy::result_large_err)]
    pub fn refresh_into(
        self,
        punnu: &sassi::Punnu<T>,
        pool: crate::pg::pool::DjogiPool,
        auth: crate::auth::AuthContext,
    ) -> Result<sassi::DeltaRefreshHandle<T>, (Self, PortablePredicateError)> {
        match self.try_portable() {
            Ok(portable) => Ok(portable.refresh_into(punnu, pool, auth)),
            Err((queryset, err)) => Err((queryset, err)),
        }
    }
}

impl<T> PortableQuerySet<T>
where
    T: crate::model::Model
        + sassi::DeltaSyncCacheable
        + crate::pg::decode::FromPgRow
        + crate::cache::DjogiDeltaSyncMeta
        + Send
        + Sync
        + 'static,
    T::Watermark: tokio_postgres::types::ToSql + tokio_postgres::types::FromSqlOwned + Sync,
    T::Id: tokio_postgres::types::ToSql + Sync,
{
    /// Construct a sassi delta-refresh subscription from a queryset whose
    /// portable-predicate gate has already passed.
    /// Infallible sibling of [`QuerySet::refresh_into`] — the public
    /// `QuerySet::refresh_into` runs `try_portable` first and either
    /// delegates here on success or returns `Err((self, err))` without
    /// touching the Punnu. Adopters who already hold a [`PortableQuerySet`]
    /// (e.g. from an explicit `try_portable` inspection) can call this
    /// directly to skip the redundant gate.
    pub fn refresh_into(
        self,
        punnu: &sassi::Punnu<T>,
        pool: crate::pg::pool::DjogiPool,
        auth: crate::auth::AuthContext,
    ) -> sassi::DeltaRefreshHandle<T> {
        // Trivially-true reductions (`BasicPredicate::True`) carry no
        // pushdown work — emitting `WHERE TRUE` on full-baseline ticks
        // would just bloat the SQL and slow planner work. Strip them
        // here so the fetcher's `Option<BasicPredicate<T>>` field
        // accurately means "non-trivial filter to push into SQL on
        // full-baseline ticks". A fresh `QuerySet<T>` initialises
        // `condition` as `Q::Portable(True)`, which the reducer hands
        // straight through as `BasicPredicate::True`.
        let PortableQuerySet {
            inner,
            portable_predicate,
        } = self;
        let empty = inner.is_empty;
        let filter = if empty {
            None
        } else {
            match portable_predicate {
                sassi::BasicPredicate::True => None,
                predicate => Some(predicate),
            }
        };
        // Capture the Punnu's event broadcast receiver before starting the
        // delta refresh. Each `refresh_into` call gets its own independent
        // receiver — per the `(Punnu, Subscription)` scope in spec §674 Knob 1.
        // Events emitted BEFORE this line (e.g., earlier `punnu.insert` calls)
        // are not visible to this subscription's receiver; only events fired
        // from this point forward are observed. That is the correct contract:
        // the warn is meant to surface LRU pressure that occurs WHILE the
        // subscription is running.
        let events_rx = std::sync::Mutex::new(punnu.events());
        let fetcher = crate::query::refresh::DjogiDeltaFetcher::<T> {
            pool,
            auth,
            empty,
            filter,
            lru_warn_issued: std::sync::atomic::AtomicBool::new(false),
            events_rx,
            // Pattern 2 outbox-tombstone watermark — first tick on an
            // events-bearing model initialises it to wall-clock `now`.
            outbox_watermark: std::sync::Mutex::new(None),
            _model: std::marker::PhantomData,
        };
        // [CHECK] Default 30s interval is a placeholder; a future change may add a
        // builder for caller-supplied interval. Pin via spec §672 review.
        let interval = std::time::Duration::from_secs(30);
        punnu.start_delta_refresh(interval, fetcher)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::ModelDescriptor;
    use crate::pg::decode::{FromJoinedPgRow, FromPgRow};
    use crate::relation::{RelationKind, RelationPath};

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

    // `qs.condition` is `Q<T>` post-flip.
    // Tests that pattern-match on the legacy `Condition` shape lower
    // through the bridge first. After this bridge is a
    // compatibility oracle for legacy parity, not the production SQL
    // emitter's input.

    #[test]
    fn new_queryset_has_no_filters() {
        let qs: QuerySet<Fake> = QuerySet::new();
        assert!(matches!(
            crate::query::q::q_to_condition_ref(&qs.condition),
            Condition::True
        ));
        assert!(qs.ordering.is_empty());
        assert!(matches!(qs.distinct, DistinctMode::None));
        assert_eq!(qs.limit, None);
        assert_eq!(qs.offset, None);
        assert!(!qs.is_empty());
    }

    #[test]
    fn none_marks_queryset_empty() {
        // `none` is an instance method: from-scratch construction goes
        // through `new.none`, but `qs.none` also works and is the
        // spelling documented at the module level.
        let qs: QuerySet<Fake> = QuerySet::<Fake>::new().none();
        assert!(qs.is_empty());
    }

    #[test]
    fn none_discards_prior_filters() {
        use crate::query::condition::{FilterValue, Leaf};
        // Any filters chained before `.none` are structurally discarded
        // the resulting queryset is always a clean empty-flagged state.
        let qs: QuerySet<Fake> = QuerySet::new()
            .filter(|_| Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true))))
            .none();
        assert!(qs.is_empty());
        assert!(matches!(
            crate::query::q::q_to_condition_ref(&qs.condition),
            Condition::True
        ));
    }

    #[test]
    fn filter_ands_onto_condition_tree() {
        use crate::query::condition::{FilterValue, Leaf};
        let qs: QuerySet<Fake> =
            QuerySet::new().filter(|_| Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true))));
        assert!(matches!(
            crate::query::q::q_to_condition_ref(&qs.condition),
            Condition::Leaf(_)
        ));
        // Second filter should AND with the first.
        let qs2 = qs.filter(|_| Condition::Leaf(Leaf::eq_raw("b", FilterValue::Bool(false))));
        match crate::query::q::q_to_condition(qs2.condition) {
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
        assert!(matches!(
            crate::query::q::q_to_condition(qs.condition),
            Condition::Not(_)
        ));
    }

    // ── `IntoQ<T>` + `filter_struct(Q<T>)` + `exclude_struct(Q<T>)` ────
    // Locks the substrate-aware filter API: trusted `IntoQ<T>` impls
    // (Q<T> directly, PortablePredicate<T>, or `{Model}Filter`
    // through the macro-emitted bridge) compose at the `Q<T>` layer.
    // Tests that compare against `Condition` explicitly lower through
    // the legacy bridge as a parity oracle.

    /// `filter_struct` accepts `Q<T>` directly. Pure-Portable Q lowers
    /// through the bridge only in this test assertion; production
    /// filtering stores the original `Q<T>` shape.
    #[test]
    fn filter_struct_accepts_q_directly() {
        use crate::query::Q;

        let q: Q<Fake> = Q::always_true();
        let qs: QuerySet<Fake> = QuerySet::new().filter_struct(q);
        // True is vacuously-true → short-circuit returns self unchanged.
        assert!(matches!(
            crate::query::q::q_to_condition(qs.condition),
            Condition::True
        ));
    }

    /// `filter_struct` over a non-vacuous Q ANDs at the `Q<T>` layer.
    /// The assertion lowers afterward through the legacy bridge to pin
    /// parity with the old false shape.
    #[test]
    fn filter_struct_q_negated_ands_onto_tree() {
        use crate::query::Q;

        // Q::always_false lowers to `Or(empty)`
        // structurally non-vacuous, so it AND-s through.
        let q: Q<Fake> = Q::always_false();
        let qs: QuerySet<Fake> = QuerySet::new().filter_struct(q);
        match crate::query::q::q_to_condition(qs.condition) {
            Condition::Or(v) => assert!(v.is_empty(), "expected Or(empty)"),
            other => panic!("expected Condition::Or(empty), got {other:?}"),
        }
    }

    /// `exclude_struct` over a portable false predicate simplifies to true
    /// through Sassi. Locks the semantic contract for pure-portable
    /// predicates: negation stays portable rather than being forced through
    /// a `Condition::Not` shell.
    #[test]
    fn exclude_struct_false_simplifies_to_true() {
        use crate::query::Q;

        let q: Q<Fake> = Q::always_false();
        let qs: QuerySet<Fake> = QuerySet::new().exclude_struct(q);
        assert!(matches!(
            crate::query::q::q_to_condition(qs.condition),
            Condition::True
        ));
    }

    /// `exclude_struct` does **not** short-circuit on
    /// vacuously-true filters. `NOT TRUE` is `FALSE`; silently
    /// dropping it would change the result set.
    #[test]
    fn exclude_struct_does_not_short_circuit_on_vacuous_true() {
        use crate::query::Q;

        let q: Q<Fake> = Q::always_true();
        let qs: QuerySet<Fake> = QuerySet::new().exclude_struct(q);
        // True AND NOT(True) → False.
        match crate::query::q::q_to_condition(qs.condition) {
            Condition::Or(parts) => assert!(parts.is_empty(), "expected false"),
            other => panic!("expected Condition::Or(empty), got {other:?}"),
        }
    }

    /// `PortablePredicate<T>` lifts through `IntoQ<T>::into_q` so
    /// `.filter_struct(my_predicate)` reads naturally without naming
    /// `Q::Portable(_)` at the callsite.
    #[test]
    fn filter_struct_accepts_portable_predicate_directly() {
        use crate::query::PortablePredicate;

        let predicate: PortablePredicate<Fake> = PortablePredicate::always_false();
        let qs: QuerySet<Fake> = QuerySet::new().filter_struct(predicate);
        match crate::query::q::q_to_condition(qs.condition) {
            Condition::Or(v) => assert!(v.is_empty()),
            other => panic!("expected Condition::Or(empty), got {other:?}"),
        }
    }

    /// **Substrate-flip lock.** `QuerySet<T>::condition` is `Q<T>`
    /// post-flip, not `Condition`. Type-level assertion: a fresh
    /// queryset's condition must be a `Q<T>` shape and lower to
    /// `Condition::True` through the bridge.
    #[test]
    fn queryset_condition_field_is_q_t() {
        let qs: QuerySet<Fake> = QuerySet::new();
        // Type-level: `qs.condition` is `Q<Fake>`, not `Condition`.
        let _: &crate::query::Q<Fake> = &qs.condition;
        // Lowering round-trip: `Q::always_true` lowers to `Condition::True`,
        // preserving SQL parity with the pre-flip queryset.
        let lowered = crate::query::q::q_to_condition(qs.condition);
        assert!(matches!(lowered, Condition::True));
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

    // ── 4 — proxy default-filter / default-order seeding ─────
    // A second hand-rolled `Model` impl that overrides the new trait
    // methods so the `QuerySet::new` seeding path can be exercised
    // without requiring the proc macro. The tests below assert that:
    // - A proxy-shaped model whose `default_filter_condition` returns
    // `Some(...)` seeds the queryset's `condition` field with that
    // value (not `Condition::True`).
    // - A proxy-shaped model whose `default_order_by` returns a
    // non-empty `Vec<OrderExpr>` seeds the `ordering` field.
    // - User `.filter(...)` calls AND-compose with the seeded condition
    // (the proxy filter is the prefix no adopter call can drop).
    // - User `.order_by(...)` calls APPEND to the seeded ordering
    // (matches the existing queryset-level append convention).
    // - The non-proxy `Fake` model above remains structurally identical
    // to its prior shape — no `RawSql` leakage when the trait
    // default impls (`None` / `Vec::new`) are used.

    /// A proxy-shaped model. The hand-rolled impl overrides
    /// `default_filter_condition` and `default_order_by`; everything
    /// else mirrors `Fake`'s `unreachable!` body.
    #[derive(Clone)]
    struct FakeProxy;
    impl crate::model::__sealed::Sealed for FakeProxy {}
    #[allow(clippy::manual_async_fn)]
    impl Model for FakeProxy {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "fake_proxy"
        }
        fn pk_value(&self) -> &Self::Pk {
            unreachable!("not called in QuerySet unit tests")
        }
        fn descriptor() -> &'static crate::descriptor::ModelDescriptor {
            unreachable!("not called in QuerySet unit tests")
        }
        fn default_filter_condition() -> Option<Condition> {
            // Mirrors the macro emission path — `Condition::__from_raw_sql_fragment`
            // is the constructor the macro uses for the lowered SQL
            // fragment. Using a static string here keeps the test self-
            // contained without dragging the macro-emission pipeline in.
            Some(Condition::__from_raw_sql_fragment("active = TRUE"))
        }
        fn default_order_by() -> Vec<crate::query::OrderExpr> {
            vec![crate::query::OrderExpr::__from_macro_column(
                "created_at",
                crate::query::Direction::Desc,
                crate::query::NullsOrder::Default,
            )]
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

    impl crate::types::Cacheable for FakeProxy {
        type Id = i64;
        type Fields = ();

        fn id(&self) -> Self::Id {
            0
        }

        fn fields() -> Self::Fields {}
    }

    impl FromPgRow for FakeProxy {
        const COLUMNS: &'static [&'static str] = &[];
        const COLUMN_LIST: &'static str = "";

        fn from_pg_row(_row: &tokio_postgres::Row) -> Result<Self, crate::DjogiError> {
            unreachable!("not called in QuerySet unit tests")
        }
    }

    impl FromJoinedPgRow for FakeProxy {
        fn from_joined_pg_row(
            _row: &tokio_postgres::Row,
            _prefix: &str,
        ) -> Result<Self, crate::DjogiError> {
            unreachable!("not called in QuerySet unit tests")
        }
    }

    /// `QuerySet::new` seeds `condition` from
    /// `Model::default_filter_condition` when the proxy override
    /// returns `Some(...)`.
    /// `qs.condition` is `Q<T>` — lower
    /// through the bridge to assert the legacy shape the SQL emitter
    /// actually sees.
    #[test]
    fn proxy_queryset_seeds_default_filter() {
        let qs: QuerySet<FakeProxy> = QuerySet::new();
        match crate::query::q::q_to_condition_ref(&qs.condition) {
            Condition::RawSql(s) => assert_eq!(s.as_str(), "active = TRUE"),
            other => panic!("expected RawSql variant, got {other:?}"),
        }
    }

    /// `QuerySet::new` seeds `ordering` from
    /// `Model::default_order_by` when the proxy override returns a
    /// non-empty Vec.
    #[test]
    fn proxy_queryset_seeds_default_order() {
        let qs: QuerySet<FakeProxy> = QuerySet::new();
        assert_eq!(qs.ordering.len(), 1);
        match &qs.ordering[0] {
            crate::query::OrderExpr::Column {
                column, direction, ..
            } => {
                assert_eq!(*column, "created_at");
                assert!(matches!(direction, crate::query::Direction::Desc));
            }
            #[allow(unreachable_patterns)]
            other => panic!("expected Column variant, got {other:?}"),
        }
    }

    /// User `.filter(...)` AND-composes with the seeded default filter
    /// the proxy condition stays as the prefix and the user's leaf is
    /// appended via the standard `Condition::and` flatten path.
    /// `qs.condition` is `Q<T>` — lower
    /// through the bridge so the assertion still inspects the shape
    /// the SQL emitter renders.
    #[test]
    fn proxy_filter_ands_with_default() {
        use crate::query::condition::{FilterValue, Leaf};
        let qs: QuerySet<FakeProxy> = QuerySet::<FakeProxy>::new()
            .filter(|_| Condition::Leaf(Leaf::eq_raw("price", FilterValue::I64(100))));
        match crate::query::q::q_to_condition_ref(&qs.condition) {
            Condition::And(parts) => {
                assert_eq!(parts.len(), 2, "expected proxy filter AND user filter");
                assert!(matches!(parts[0], Condition::RawSql(_)));
                assert!(matches!(parts[1], Condition::Leaf(_)));
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    /// User `.order_by(...)` APPENDS to the seeded default ordering
    /// the proxy ordering stays as the prefix and the user's expression
    /// is pushed onto the end. Matches the existing queryset convention
    /// (`queryset.rs:25-28`).
    #[test]
    fn proxy_order_by_appends_to_default() {
        let user_order = crate::query::OrderExpr::__from_macro_column(
            "id",
            crate::query::Direction::Asc,
            crate::query::NullsOrder::Default,
        );
        let qs: QuerySet<FakeProxy> = QuerySet::<FakeProxy>::new().order_by(|_| user_order);
        assert_eq!(qs.ordering.len(), 2);
        // Prefix: the seeded default.
        match &qs.ordering[0] {
            crate::query::OrderExpr::Column { column, .. } => assert_eq!(*column, "created_at"),
            #[allow(unreachable_patterns)]
            other => panic!("expected Column, got {other:?}"),
        }
        // Suffix: the user's `.order_by(...)`.
        match &qs.ordering[1] {
            crate::query::OrderExpr::Column { column, .. } => assert_eq!(*column, "id"),
            #[allow(unreachable_patterns)]
            other => panic!("expected Column, got {other:?}"),
        }
    }

    /// Mutation read-tail guard must allow model/proxy default ordering.
    /// Default ordering is seeded by `QuerySet::new` and should not be
    /// treated as an explicit user-specified `order_by`.
    #[test]
    fn mutation_guard_allows_proxy_default_ordering() {
        let qs: QuerySet<FakeProxy> = QuerySet::new();
        assert!(
            qs.validate_mutation_read_tail("delete").is_ok(),
            "proxy default ordering must not trip mutation guard"
        );
    }

    /// Calling `.order_by(...)` marks ordering as explicit and must trip
    /// mutation guard, even when proxy default ordering is also present.
    #[test]
    fn mutation_guard_rejects_explicit_order_by() {
        let user_order = crate::query::OrderExpr::__from_macro_column(
            "id",
            crate::query::Direction::Asc,
            crate::query::NullsOrder::Default,
        );
        let qs: QuerySet<FakeProxy> = QuerySet::<FakeProxy>::new().order_by(|_| user_order);
        let err = qs
            .validate_mutation_read_tail("delete")
            .expect_err("explicit order_by should be rejected for mutation terminals");
        match err {
            crate::DjogiError::Validation(msg) => {
                assert!(
                    msg.contains("order_by"),
                    "validation should mention order_by, got: {msg}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn mutation_guard_rejects_other_read_tail_modifiers() {
        for (qs, expected) in [
            (QuerySet::<Fake>::new().offset(10), "offset"),
            (QuerySet::<Fake>::new().distinct(), "distinct"),
            (QuerySet::<Fake>::new().select_for_update(), "row locks"),
        ] {
            let err = qs
                .validate_mutation_read_tail("delete")
                .expect_err("read-tail modifier should be rejected for mutation terminals");
            match err {
                crate::DjogiError::Validation(msg) => {
                    assert!(
                        msg.contains(expected),
                        "validation should mention {expected}, got: {msg}"
                    );
                }
                other => panic!("expected Validation, got {other:?}"),
            }
        }
    }

    fn fake_relation_path() -> RelationPath<Fake, FakeProxy> {
        RelationPath::new("proxy_id", "fake_proxy", RelationKind::ForeignKey)
    }

    #[test]
    fn mutation_guard_rejects_prefetch_paths() {
        let qs: QuerySet<Fake> = QuerySet::new().prefetch(fake_relation_path());
        let err = qs
            .validate_mutation_read_tail("delete")
            .expect_err("prefetch should be rejected for mutation terminals");
        match err {
            crate::DjogiError::Validation(msg) => {
                assert!(
                    msg.contains("prefetch"),
                    "validation should mention prefetch, got: {msg}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn mutation_guard_rejects_select_related_paths() {
        let qs: QuerySet<Fake> = QuerySet::new().select_related(fake_relation_path());
        let err = qs
            .validate_mutation_read_tail("update")
            .expect_err("select_related should be rejected for mutation terminals");
        match err {
            crate::DjogiError::Validation(msg) => {
                assert!(
                    msg.contains("select_related"),
                    "validation should mention select_related, got: {msg}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn mutation_guard_rejects_cache_target() {
        let punnu = crate::cache::Punnu::<FakeProxy>::builder().build();
        let qs: QuerySet<FakeProxy> = QuerySet::new().bind_cache_for_test(punnu);
        let err = qs
            .validate_mutation_read_tail("delete")
            .expect_err("cache_target should be rejected for mutation terminals");
        match err {
            crate::DjogiError::Validation(msg) => {
                assert!(
                    msg.contains("cache_target"),
                    "validation should mention cache_target, got: {msg}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    /// The non-proxy `Fake` model is structurally unchanged —
    /// `default_filter_condition` returns `None` (default impl) and
    /// `default_order_by` returns the empty `Vec`, so the seeded queryset
    /// is identical to the prior shape (`Condition::True` + empty
    /// ordering).
    /// The substrate is now `Q<T>`. For
    /// `default_filter_condition == None`, `QuerySet::new` seeds
    /// `Q::always_true` (== `Q::Portable(True)`), which
    /// the bridge lowers to the legacy `Condition::True` — preserving
    /// the pre-flip emission contract.
    #[test]
    fn non_proxy_queryset_unchanged_by_t3_4() {
        let qs: QuerySet<Fake> = QuerySet::new();
        assert!(matches!(
            crate::query::q::q_to_condition_ref(&qs.condition),
            Condition::True
        ));
        assert!(qs.ordering.is_empty());
    }

    // ── group_by_region / count_by_region type-dispatch tests ────────────
    // These tests confirm the entry-point signatures compile and return the
    // expected type shapes. The SQL emission shape is tested in sql.rs.

    // ── once-warn counter test ────────────────────────────────────────────────
    // Verifies that calling `group_by_region` against an unindexed region model
    // emits at most one `tracing::warn!` regardless of how many times the method
    // is called. The guard uses `std::sync::Once` which is process-wide; the
    // counter is captured with `tracing_test::traced_test` scoped to this test's
    // thread-local subscriber.
    // `ONCE` fires at most once per process run. `#[traced_test]` captures the
    // warn if it fires inside this test invocation and zero otherwise. The
    // assertion uses `<=` rather than `==` because test parallelism may cause
    // another unindexed call (in a different test) to consume the `Once` first
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

    // ── cluster_by_proximity / bucket_by_cell type-dispatch tests ────────

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

    // ── into_basic_predicate: conservative Q<T>→BasicPredicate<T> walk ──
    // These tests set `qs.condition` directly (via `pub(crate)` access) to
    // exercise every reducible and unreducible shape. The integration test
    // (`basic_predicate_extraction.rs`) covers the externally
    // observable legacy-filter behavior (`Condition` path → None + warn,
    // unfiltered QuerySet → Some(True)).

    /// A fresh `QuerySet::new` starts as `Q::Portable(True)`.
    /// `into_basic_predicate` must return `Some(BasicPredicate::True)`.
    #[test]
    fn into_basic_predicate_unfiltered_returns_true() {
        let qs: QuerySet<Fake> = QuerySet::new();
        // Verify the initial condition is portable before calling.
        assert!(
            matches!(&qs.condition, Q::Portable(_)),
            "unfiltered QuerySet must start as Q::Portable (substrate regression?)",
        );
        let result = qs.into_basic_predicate();
        assert!(
            matches!(result, Some(sassi::BasicPredicate::True)),
            "unfiltered QuerySet should reduce to Some(BasicPredicate::True)"
        );
    }

    /// A QuerySet with `condition = Q::Portable(False)` reduces
    /// to `Some(BasicPredicate::False)`. Verifies the `Q::Portable(p)` arm works
    /// for non-True sentinels.
    #[test]
    fn into_basic_predicate_portable_false_reduces() {
        let mut qs: QuerySet<Fake> = QuerySet::new();
        qs.condition = Q::always_false();
        let result = qs.into_basic_predicate();
        assert!(
            matches!(result, Some(sassi::BasicPredicate::False)),
            "Q::Portable(False) should reduce to Some(BasicPredicate::False)"
        );
    }

    /// A QuerySet with `Q::Compound { And, [Portable(True), Portable(False)] }`
    /// reduces to `Some(BasicPredicate::And(vec![True, False]))`.
    /// Verifies the Compound-And arm walks all parts and assembles the
    /// `BasicPredicate::And` aggregator.
    #[test]
    fn into_basic_predicate_compound_and_reduces() {
        use crate::query::q::CompoundOp;
        let mut qs: QuerySet<Fake> = QuerySet::new();
        qs.condition = Q::Compound {
            op: CompoundOp::And,
            parts: vec![Q::always_true(), Q::always_false()],
        };
        let result = qs.into_basic_predicate();
        match result {
            Some(sassi::BasicPredicate::And(parts)) => {
                assert_eq!(parts.len(), 2, "And predicate should have exactly 2 parts");
                assert!(matches!(parts[0], sassi::BasicPredicate::True));
                assert!(matches!(parts[1], sassi::BasicPredicate::False));
            }
            other => panic!(
                "expected Some(BasicPredicate::And([True, False])), got {}",
                if other.is_some() {
                    "Some(non-And variant)"
                } else {
                    "None"
                }
            ),
        }
    }

    /// A QuerySet with `Q::Compound { Or, [Portable(True), Portable(False)] }`
    /// reduces to `Some(BasicPredicate::Or(vec![True, False]))`.
    /// Verifies the Compound-Or arm.
    #[test]
    fn into_basic_predicate_compound_or_reduces() {
        use crate::query::q::CompoundOp;
        let mut qs: QuerySet<Fake> = QuerySet::new();
        qs.condition = Q::Compound {
            op: CompoundOp::Or,
            parts: vec![Q::always_true(), Q::always_false()],
        };
        let result = qs.into_basic_predicate();
        match result {
            Some(sassi::BasicPredicate::Or(parts)) => {
                assert_eq!(parts.len(), 2, "Or predicate should have exactly 2 parts");
            }
            _ => panic!("expected Some(BasicPredicate::Or([True, False]))"),
        }
    }

    /// `Q::Negated(Q::Portable(p))` reduces to `Some(BasicPredicate::Not(Box::new(p)))`.
    /// Verifies the Negated arm: pure-Basic inner wrapped in Not.
    #[test]
    fn into_basic_predicate_negated_portable_reduces() {
        let mut qs: QuerySet<Fake> = QuerySet::new();
        qs.condition = Q::Negated(Box::new(Q::always_true()));
        let result = qs.into_basic_predicate();
        match result {
            Some(sassi::BasicPredicate::Not(inner)) => {
                assert!(
                    matches!(*inner, sassi::BasicPredicate::True),
                    "inner of Not should be True"
                );
            }
            _ => panic!("expected Some(BasicPredicate::Not(True))"),
        }
    }

    /// `Q::Compound { And, [Portable(True), Ilike(...)] }` is Unreducible — the
    /// Ilike part is SQL-only. Returns `None`. Verifies short-circuit on
    /// first unreducible part.
    #[test]
    fn into_basic_predicate_compound_with_ilike_refuses() {
        use crate::query::field::FieldRef;
        use crate::query::q::CompoundOp;
        let mut qs: QuerySet<Fake> = QuerySet::new();
        qs.condition = Q::Compound {
            op: CompoundOp::And,
            parts: vec![
                Q::always_true(),
                // ILIKE is SQL-only — cannot be expressed as BasicPredicate.
                Q::Ilike(FieldRef::<Fake, String>::new("label"), "foo%".to_string()),
            ],
        };
        let result = qs.into_basic_predicate();
        assert!(
            result.is_none(),
            "Q::Compound containing Q::Ilike must reduce to None"
        );
    }

    /// `Q::Condition(...)` is always Unreducible. Ordinary
    /// `.filter(|f| f.col.eq(...))` closures and portable generated
    /// `{Model}Filter` clauses produce `Q::Portable`, but SQL-only generated
    /// filter clauses and any closure that hand-rolls a raw `Condition` still
    /// route through `Q::Condition(_)`. This test pins that SQL-only escape
    /// hatch.
    #[test]
    fn into_basic_predicate_legacy_condition_refuses() {
        use crate::query::condition::{FilterValue, Leaf};
        let qs: QuerySet<Fake> =
            QuerySet::new().filter(|_| Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true))));
        // The closure returns a raw `Condition`, which the legacy
        // `IntoQ for Condition` shim wraps as `Q::Condition(...)`.
        assert!(
            matches!(&qs.condition, Q::Condition(_)),
            "raw Condition payloads must reach the queryset as Q::Condition (regression in and_condition_into_q?)",
        );
        let result = qs.into_basic_predicate();
        assert!(
            result.is_none(),
            "Q::Condition (legacy filter path) must reduce to None"
        );
    }

    /// `Q::Negated(Q::Condition(...))` is Unreducible — the inner is not Basic.
    /// Returns `None`. Verifies that Negated propagates Unreducible from inner.
    #[test]
    fn into_basic_predicate_negated_non_basic_refuses() {
        use crate::query::condition::{FilterValue, Leaf};
        let inner_condition = Condition::Leaf(Leaf::eq_raw("x", FilterValue::Bool(false)));
        let mut qs: QuerySet<Fake> = QuerySet::new();
        qs.condition = Q::Negated(Box::new(Q::Condition(inner_condition)));
        let result = qs.into_basic_predicate();
        assert!(
            result.is_none(),
            "Q::Negated(Q::Condition(...)) must reduce to None"
        );
    }

    /// `Q::Xor(Q::Portable(True), Q::Portable(False))` reduces to
    /// `BasicPredicate::Xor(...)`.
    #[test]
    fn into_basic_predicate_xor_reduces() {
        let mut qs: QuerySet<Fake> = QuerySet::new();
        qs.condition = Q::Xor(Box::new(Q::always_true()), Box::new(Q::always_false()));
        let result = qs.into_basic_predicate();
        match result {
            Some(sassi::BasicPredicate::Xor(left, right)) => {
                assert!(matches!(*left, sassi::BasicPredicate::True));
                assert!(matches!(*right, sassi::BasicPredicate::False));
            }
            _ => panic!("expected Some(BasicPredicate::Xor(True, False))"),
        }
    }

    // ── PR4 — portable cache/refresh gate API ─────────────────────────────────
    // These tests pin the new public-surface invariants introduced when
    // `QuerySet::cache` and `QuerySet::refresh_into` flipped to `Result`-
    // returning gates. The integration test `phase8eta_pr4_cache_refresh_gate`
    // covers the database-backed behavior; these unit tests cover the
    // structural reduction without a DB round-trip so regressions surface
    // even when the integration suite is skipped.

    /// `QuerySet::none` flips `is_empty = true`. Both `try_portable` and
    /// `is_portable` must accept it as a portable-false predicate so adopters
    /// can short-circuit authorization branches through `.none.cache(...)`.
    #[test]
    fn try_portable_accepts_none_as_portable_false() {
        let qs: QuerySet<Fake> = QuerySet::new().none();
        assert!(qs.is_portable().is_ok());
        let portable = qs.try_portable().expect("none() must satisfy try_portable");
        assert!(matches!(portable.predicate(), sassi::BasicPredicate::False));
        // `into_query_set` round-trips back to a queryset that still
        // short-circuits at terminals via `is_empty`.
        assert!(portable.into_query_set().is_empty);
    }

    /// `try_portable` on a fresh `QuerySet::new` (Q::Portable(True))
    /// reduces and the resulting `PortableQuerySet::predicate` is True.
    #[test]
    fn try_portable_unfiltered_yields_true_predicate() {
        let qs: QuerySet<Fake> = QuerySet::new();
        let portable = qs
            .try_portable()
            .expect("unfiltered queryset must satisfy try_portable");
        assert!(matches!(portable.predicate(), sassi::BasicPredicate::True));
    }

    /// `try_portable` returns the typed `CacheInvalidNode` error and hands
    /// the original queryset back unchanged when the condition tree carries
    /// SQL-only legacy `Q::Condition` payloads.
    #[test]
    fn try_portable_rejects_legacy_condition_payload() {
        use crate::query::condition::{FilterValue, Leaf};
        let qs: QuerySet<Fake> =
            QuerySet::new().filter(|_| Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true))));
        // `is_portable` reports the same classification through the
        // borrow-style API.
        assert!(matches!(
            qs.is_portable(),
            Err(PortablePredicateError::CacheInvalidNode { kind: "Condition" }),
        ));
        let (recovered, err) = qs
            .try_portable()
            .expect_err("legacy Condition must fail try_portable");
        assert!(matches!(
            err,
            PortablePredicateError::CacheInvalidNode { kind: "Condition" }
        ));
        // The recovered queryset still carries the legacy condition so the
        // caller can rewrite it without losing the original filter.
        assert!(matches!(&recovered.condition, Q::Condition(_)));
    }
}
