//! `DjogiPool` — the framework's Postgres connection pool.
//! # What
//! `DjogiPool` wraps a `deadpool_postgres::Pool` and is the single way Djogi
//! checks out a Postgres connection. The type is `Clone` (the inner pool is
//! `Arc`-backed) and `Send + Sync`, so a `DjogiPool` can be shared across
//! tasks without external wrapping.
//! # Why a public ergonomic surface
//! `DjogiPool::connect(url)` is fine for development, but production
//! services need three things on top:
//! 1. **Tunable size and timeout.** The default `max_size = 5` is fine
//!    for development; production sizes the pool against expected
//!    concurrent database-touching tasks and bounds the wait for a slot
//!    so saturated pools fail fast.
//! 2. **A per-physical-connection setup hook.** `SET heer.node_id`,
//!    `SET application_name`, `SET statement_timeout` belong on every
//!    fresh connection; running them on every checkout would conflict
//!    with [`DjogiContext::set_tenant`](crate::context::DjogiContext::set_tenant)'s
//!    transaction-local `set_config('app.tenant_id', $1, true)`.
//! 3. **A raw-client escape hatch.** `COPY`, server-side cursors,
//!    `CREATE EXTENSION`, and third-party crates that take a
//!    `&tokio_postgres::Client` cannot route through `DjogiContext`.
//!    The raw-driver bypass is dirty-by-default: a closure that
//!    returns `Err`, panics, or is cancelled detaches the connection
//!    from the pool so a poisoned session (open transaction,
//!    uncommitted `SET ROLE`, advisory lock) cannot leak to the next
//!    checkout. Adopter code reaches the bypass through the sealed
//!    [`RawPoolAccessExt::raw_with_client`](crate::__bypass::RawPoolAccessExt)
//!    trait; the inherent `DjogiPool::with_client` method is
//!    `pub(crate)` and used only by internal substrate. See the
//!    [raw SQL escape hatches spec](crate::__bypass) for the broader
//!    contract.
//!    [`DjogiPool::builder`] (`.max_size`, `.timeout`, `.post_connect`) covers
//!    the first two; the raw-driver bypass is reached via
//!    [`RawPoolAccessExt::raw_with_client`](crate::__bypass::RawPoolAccessExt).
//!    `connect(url)` is preserved as sugar for
//!    `DjogiPool::builder(url).build().await`. COPY, server-side cursors,
//!    and other direct-driver protocol operations intentionally remain behind
//!    the explicit [`RawPoolAccessExt::raw_with_client`](crate::__bypass::RawPoolAccessExt)
//!    bypass rather than a separate typed wrapper.
//! # Where the `post_connect` hook fits in deadpool's lifecycle
//! The hook is wired through deadpool's `PoolBuilder::post_create`, which
//! fires only after `Manager::create` materialises a new physical
//! connection — i.e. on initial pool fill, after a recycle failure that
//! discards a stale connection, or whenever pool growth opens a fresh
//! slot. It does **not** fire on the checkout path that reuses an existing
//! physical connection. Per-checkout reset is a separate concern that a
//! future release may expose; the v0.1.0 API is deliberately limited to
//! create-time setup so it composes cleanly with transaction-local GUCs
//! (notably `DjogiContext::set_tenant`, which uses
//! `set_config('app.tenant_id', $1, true)` and expects a clean session at
//! the start of every transaction).

use crate::pg::connection::PgConnection;
use crate::{DbError, DjogiError};
use deadpool_postgres::{Config, ManagerConfig, PoolConfig, RecyclingMethod, Runtime};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio_postgres::NoTls;

/// Per-process monotonic counter for [`DjogiPool::pool_id`].
/// Allocated lazily by [`next_pool_id`]; starts at 1 so a freshly-zeroed
/// `pool_id` is recognisably "uninitialised" if it ever surfaces in a
/// debug print.
static NEXT_POOL_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate the next per-process pool id. Crate-private so internal
/// substrate that constructs `DjogiPool` by struct literal (e.g. the
/// audit-side pool in `migrate/runner.rs`) can preserve the
/// "every freshly-built pool gets a unique id" invariant without
/// reaching into the atomic directly.
pub(crate) fn next_pool_id() -> u64 {
    NEXT_POOL_ID.fetch_add(1, Ordering::Relaxed)
}

/// Default `max_size` when the caller does not override it.
/// Five matches the original Phase-1 hard-coded value and keeps every
/// existing test/callsite that uses [`DjogiPool::connect`] running against
/// the same pool size as before. Production deployments override this
/// through [`DjogiPoolBuilder::max_size`] or by reading
/// `[database].max_connections` from `Djogi.toml` via
/// [`DjogiPool::from_database_config`].
pub const DEFAULT_MAX_SIZE: usize = 5;

/// Environment-variable name read by [`DjogiPool::from_database_config`].
/// When set to a positive integer, it overrides `[database].max_connections`
/// from `Djogi.toml` and the builder default — i.e. it sits at the top of
/// the resolution chain (env > Djogi.toml > builder default).
/// Empty / unparseable / `0` values fall through to the next layer rather
/// than zeroing the pool — a typo must not silently disable the database.
pub const ENV_DATABASE_MAX_CONNECTIONS: &str = "DJOGI_DATABASE_MAX_CONNECTIONS";

/// Boxed future returned by closures handed to the raw-driver bypass
/// both `RawPoolAccessExt::raw_with_client` (the adopter-facing path)
/// and the internal `DjogiPool::with_client` (`pub(crate)`) it dispatches
/// to.
/// Exposed at module scope so adopters can spell the lifetime explicitly
/// when factoring a closure body into a named function:
/// ```ignore
/// use djogi::__bypass::RawPoolAccessExt as _;
///
/// fn install_extension<'a>(
///     client: &'a mut tokio_postgres::Client,
/// ) -> ClientFuture<'a, ()> {
///     Box::pin(async move {
///         client
///             .batch_execute("CREATE EXTENSION IF NOT EXISTS postgis")
///             .await
///             .map_err(djogi::DjogiError::from)
///     })
/// }
///
/// pool.raw_with_client(install_extension).await?;
/// ```
/// Adopters that already use inline `Box::pin(async move { ... })` blocks
/// rarely need to spell this alias themselves — the existential `Future`
/// produced by an `async` block satisfies the bound directly. The alias
/// exists for the cases where adopters factor the closure body out into a
/// named function (helpful for long migrations or when the same setup is
/// reused across multiple pools).
pub type ClientFuture<'a, R> = Pin<Box<dyn Future<Output = Result<R, DjogiError>> + Send + 'a>>;

/// Public alias for a `post_connect` closure's returned future.
/// Mirrors [`ClientFuture`] but pins the result type to `()`, since
/// `post_connect` runs setup-only SQL and has no return value beyond
/// "succeeded" or "failed".
pub type PostConnectFuture<'a> = ClientFuture<'a, ()>;

/// Boxed `post_connect` closure stored on the builder. Invoked from
/// deadpool's `post_create` hook — runs once per physical connection
/// creation, never on the per-checkout path.
type PostConnectFn =
    Arc<dyn for<'a> Fn(&'a mut tokio_postgres::Client) -> PostConnectFuture<'a> + Send + Sync>;

/// The framework's Postgres connection pool.
/// Wraps `deadpool_postgres::Pool` with a Djogi-specific constructor.
/// `Clone` is free — the underlying pool is `Arc`-backed, so a clone bumps
/// a refcount rather than copying the pool state.
#[derive(Clone)]
pub struct DjogiPool {
    /// The underlying deadpool-postgres pool. `pub(crate)` so internal
    /// substrate (`context`, `transaction`, `live_migrate`, `outbox`) can
    /// reach the inner pool without making the whole inner type part of
    /// the public API surface. Adopter code goes through the public
    /// methods on `DjogiPool` and through `DjogiContext`, never through
    /// `inner`.
    pub(crate) inner: deadpool_postgres::Pool,
    /// The Postgres connection URL the pool was built with. Retained so
    /// internal substrate that needs a dedicated connection outside the
    /// pool — notably the NOTIFY listener, which can't subscribe to
    /// `tokio_postgres::AsyncMessage` on a pooled connection — can issue
    /// a fresh `tokio_postgres::connect` against the same URL.
    /// `None` for pools that adopt a pre-built `deadpool_postgres::Pool`
    /// without exposing a URL (e.g. the audit-side pool in
    /// `migrate/runner.rs`). NOTIFY listener spawn against a URL-less
    /// pool returns `NotifyError::ListenerStartFailed`.
    #[allow(dead_code)] // only read under `feature = "notify"`
    pub(crate) url: Option<String>,
    /// Per-process unique identity, copied verbatim on `Clone`, so cloned
    /// handles share an id (and any per-pool keyed state — listener
    /// registry, future statement caches) while freshly-built pools get
    /// fresh ids. `&pool.inner as *const _` is NOT a stable identity
    /// `deadpool_postgres::Pool` is Arc-shaped, so cloning a `DjogiPool`
    /// produces a new outer struct address while the inner allocation is
    /// shared. The notify registry needs share-on-clone semantics, hence
    /// the explicit id.
    #[allow(dead_code)] // only read under `feature = "notify"`
    pub(crate) pool_id: u64,
}

impl std::fmt::Debug for DjogiPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DjogiPool")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

/// Snapshot of a [`DjogiPool`]'s counters.
/// Returned by [`DjogiPool::status`]. Wraps the deadpool-postgres status so
/// adopters depend on Djogi's surface, not the underlying pool crate.
#[derive(Debug, Clone, Copy)]
pub struct DjogiPoolStatus {
    /// Configured upper bound on physical connections.
    pub max_size: usize,
    /// Physical connections opened so far (idle + in-use).
    pub size: usize,
    /// Idle connections ready for immediate checkout.
    pub available: usize,
    /// Tasks currently waiting for a checkout slot.
    pub waiting: usize,
}

impl DjogiPoolStatus {
    /// Build a snapshot from the underlying deadpool status. Crate-private
    /// so the conversion does not surface deadpool's type on Djogi's
    /// public API; adopters reach a snapshot only via [`DjogiPool::status`].
    pub(crate) fn from_deadpool(s: deadpool_postgres::Status) -> Self {
        Self {
            max_size: s.max_size,
            size: s.size,
            available: s.available,
            waiting: s.waiting,
        }
    }
}

impl DjogiPool {
    /// Build a `DjogiPool` from a Postgres connection URL using framework
    /// defaults.
    /// Equivalent to `DjogiPool::builder(url).build().await`. Defaults:
    /// - `max_size` = [`DEFAULT_MAX_SIZE`] (5)
    /// - no wait timeout (callers block until a slot is available)
    /// - no `post_connect` hook
    ///   For tunable size, timeouts, or per-physical-connection setup use
    ///   [`DjogiPool::builder`] instead.
    /// ```ignore
    /// let pool = djogi::pg::pool::DjogiPool::connect(&database_url).await?;
    /// let mut ctx = djogi::DjogiContext::from_pool(pool);
    /// ```
    pub async fn connect(url: &str) -> Result<Self, DjogiError> {
        Self::builder(url).build().await
    }

    /// Start configuring a `DjogiPool` against the given Postgres URL.
    /// The URL format is the standard Postgres connection string, e.g.
    /// `postgres://user:pass@localhost:5432/dbname`.
    /// The returned [`DjogiPoolBuilder`] exposes `.max_size`, `.timeout`,
    /// and `.post_connect`, finalised by `.build().await`.
    /// ```ignore
    /// let pool = DjogiPool::builder("postgres://localhost/app")
    ///     .max_size(20)
    ///     .timeout(Duration::from_secs(5))
    ///     .build()
    ///     .await?;
    /// ```
    pub fn builder(url: impl Into<String>) -> DjogiPoolBuilder {
        DjogiPoolBuilder::new(url.into())
    }

    /// Build a `DjogiPool` from a loaded
    /// [`DatabaseConfig`](crate::config::DatabaseConfig) applying the
    /// standard env > Djogi.toml > builder-default precedence.
    /// The resolution chain for `max_size` is:
    /// 1. The `DJOGI_DATABASE_MAX_CONNECTIONS` environment variable
    ///    ([`ENV_DATABASE_MAX_CONNECTIONS`]), if set and parseable as a
    ///    positive integer. Wins over everything.
    /// 2. `[database].max_connections` from the loaded config, if non-zero.
    /// 3. Builder default ([`DEFAULT_MAX_SIZE`]).
    ///    `database.url` from the loaded config supplies the URL. Note that
    ///    [`DjogiConfig::load`](crate::config::DjogiConfig::load) already lifts
    ///    the `DATABASE_URL` env var into `database.url`, so the URL
    ///    resolution chain is handled at the config layer — this method
    ///    only owns the `max_size` chain.
    ///    Callers that want to override the URL specifically (without going
    ///    through `DjogiConfig::load`) should use [`DjogiPool::builder`]
    ///    directly.
    /// # Example
    /// ```ignore
    /// let cfg = djogi::DjogiConfig::load()?;
    /// let pool = djogi::pg::pool::DjogiPool::from_database_config(&cfg.database).await?;
    /// ```
    /// # No `post_connect` hook is attached
    /// `from_database_config` is the config-driven entry point — it does
    /// not register a [`DjogiPoolBuilder::post_connect`] hook. Adopters
    /// that need a hook should chain through [`DjogiPool::builder`] and
    /// merge the env / config resolution into their own builder
    /// invocation, e.g.:
    /// ```ignore
    /// let cfg = djogi::DjogiConfig::load()?;
    /// let mut b = djogi::pg::pool::DjogiPool::builder(&cfg.database.url);
    /// if let Some(n) = djogi::pg::pool::resolve_max_connections(&cfg.database) {
    ///     b = b.max_size(n);
    /// }
    /// let pool = b
    ///     .post_connect(|client| Box::pin(async move {
    ///         client.batch_execute("SET application_name = 'web'").await?;
    ///         Ok(())
    ///     }))
    ///     .build()
    ///     .await?;
    /// ```
    pub async fn from_database_config(
        cfg: &crate::config::DatabaseConfig,
    ) -> Result<Self, DjogiError> {
        let mut builder = Self::builder(&cfg.url);
        if let Some(n) = resolve_max_connections(cfg) {
            builder = builder.max_size(n);
        }
        builder.build().await
    }

    /// Return a snapshot of the pool's counters.
    /// The returned [`DjogiPoolStatus`] carries `max_size` (configured
    /// upper bound), `size` (physical connections opened), and
    /// `available` (idle connections ready for checkout). The call is a
    /// cheap snapshot read — it does not lock the pool or block on
    /// in-flight checkouts.
    /// ```ignore
    /// let status = pool.status();
    /// tracing::info!(
    ///     max_size = status.max_size,
    ///     size = status.size,
    ///     available = status.available,
    ///     waiting = status.waiting,
    ///     "pool snapshot",
    /// );
    /// ```
    pub fn status(&self) -> DjogiPoolStatus {
        DjogiPoolStatus::from_deadpool(self.inner.status())
    }

    /// Acquire a `PgConnection` from the pool.
    /// The returned `PgConnection` holds the connection for the caller's
    /// lifetime and returns it to the pool on drop. If the pool has no idle
    /// connections and is at max capacity, this waits until one is available
    /// (deadpool default: indefinitely; bound the wait via
    /// [`DjogiPoolBuilder::timeout`]).
    #[allow(clippy::disallowed_methods)]
    pub(crate) async fn get(&self) -> Result<PgConnection, DjogiError> {
        let obj = self.inner.get().await.map_err(map_pool_err)?;
        Ok(PgConnection::new(obj))
    }

    /// Borrow a raw `tokio_postgres::Client` for the duration of a
    /// closure.
    /// # When to reach for this
    /// `with_client` is the explicit escape hatch for operations that
    /// **cannot** route through [`DjogiContext`](crate::context::DjogiContext):
    /// - `COPY FROM STDIN` / `COPY TO STDOUT` and other binary-protocol
    ///   features that need direct driver access.
    /// - Server-side cursors that streaming consumers drive via the
    ///   driver API rather than through `QuerySet::stream`.
    /// - `CREATE EXTENSION` and other DDL that runs once at cold-start
    ///   migration time, before any model context exists.
    /// - Bridging into third-party crates that take a
    ///   `&tokio_postgres::Client` directly (rare since
    ///   collapsed the HeeRanjID + extension install path through
    ///   `djogi::migrate::bootstrap::run_phase_zero`, which itself
    ///   takes a generic client).
    /// # When NOT to reach for this
    /// **`with_client` is NOT for raw `SELECT` queries.** Adopter code
    /// that needs a raw query should use
    /// [`DjogiContext::raw_query`](crate::context::DjogiContext::raw_query) /
    /// [`DjogiContext::raw_execute`](crate::context::DjogiContext::raw_execute),
    /// which keep the call inside the framework's pool / transaction
    /// substrate, surface decode helpers, and compose with
    /// [`atomic`](crate::transaction::atomic) scopes (so the raw query
    /// participates in the same transaction as the surrounding model
    /// operations).
    /// # Connection lifecycle — clean exit returns, dirty exit detaches
    /// The connection is checked out before the closure runs. The
    /// outcome path determines what happens to it on the way out:
    /// - **Clean exit (`Ok`).** The `Object` drops normally and
    ///   deadpool returns the connection to the pool. The next
    ///   checkout reuses the same physical connection.
    /// - **Dirty exit (`Err`, panic, future cancellation).** The
    ///   `Object` is detached via `deadpool::managed::Object::take`,
    ///   which removes it from the pool's tracker; the underlying
    ///   `ClientWrapper` is dropped immediately, closing the
    ///   `tokio_postgres::Client` and the underlying socket. The
    ///   pool will create a fresh physical connection on the next
    ///   demand. This is the safe-by-default semantic — the closure
    ///   cannot leak a poisoned session (open transaction,
    ///   uncommitted `SET ROLE`, advisory lock, half-finished `COPY`
    ///   protocol stream) into the next checkout.
    ///   The dirty-exit detach is important because Djogi's pool runs
    ///   `deadpool_postgres::RecyclingMethod::Fast`, which only checks
    ///   `is_closed()` on return — it does NOT run `ROLLBACK`,
    ///   `RESET ALL`, or `DISCARD ALL`. Without the dirty-exit detach
    ///   here, an `Err`/panic during a `BEGIN` block or a `SET ROLE`
    ///   would silently hand the next request a backend with the wrong
    ///   role/transaction state. The trade-off is one extra physical
    ///   connection per dirty exit, which is the right cost to pay for
    ///   the safety guarantee.
    /// # Session-affecting commands
    /// Even on the **clean-exit path**, session-level state set inside
    /// the closure (`SET ROLE`, `SET search_path`, advisory locks,
    /// prepared statements outside the statement cache) leaves the
    /// connection in a non-default state when it returns to the pool.
    /// Prefer transaction-local settings (`SET LOCAL ...`,
    /// `set_config(name, value, true)`, `BEGIN; ... COMMIT;`) or
    /// reset what you set before the closure resolves. The recycler
    /// does NOT issue `DISCARD ALL` on return — that is a deliberate
    /// trade-off (recycler latency cost vs cleanliness) inherited
    /// from `deadpool_postgres`'s default `RecyclingMethod::Fast`.
    /// # Example
    /// ```ignore
    /// pool.with_client(|client| {
    ///     Box::pin(async move {
    ///         client
    ///             .batch_execute("CREATE EXTENSION IF NOT EXISTS postgis")
    ///             .await
    ///             .map_err(djogi::DjogiError::from)
    ///     })
    /// }).await?;
    /// ```
    #[allow(clippy::disallowed_methods)]
    pub(crate) async fn with_client<F, R>(&self, f: F) -> Result<R, DjogiError>
    where
        F: for<'a> FnOnce(&'a mut tokio_postgres::Client) -> ClientFuture<'a, R>,
    {
        // The guard holds the deadpool `Object` and starts in the
        // "dirty" state. Only after the closure returns `Ok` do we
        // clear the dirty flag and let the guard's `Drop` return
        // the connection to the pool normally. Any other exit path
        // (closure returns `Err`, closure panics during the await,
        // the future is dropped before completing) triggers the
        // detach branch in `Drop` and the connection is discarded.
        let obj = self.inner.get().await.map_err(map_pool_err)?;
        let mut guard = WithClientGuard {
            obj: Some(obj),
            committed: false,
        };

        let result = {
            let obj_ref = guard
                .obj
                .as_mut()
                .expect("guard.obj is Some until commit/drop");
            let client: &mut tokio_postgres::Client = obj_ref;
            f(client).await
        };

        if result.is_ok() {
            guard.committed = true;
        }
        drop(guard);

        result
    }
}

// ---------------------------------------------------------------------------
// `with_client` lifecycle guard
// ---------------------------------------------------------------------------

/// RAII guard around a checked-out deadpool `Object` for the duration of
/// a [`DjogiPool::with_client`] call.
/// The guard is **dirty-by-default**: only an explicit `committed = true`
/// flip (set after the closure returns `Ok`) lets the connection return
/// to the pool. Any other exit path — closure `Err`, panic during the
/// `await`, future cancellation that drops the `with_client` frame
/// before completion — leaves `committed = false` and the `Drop` impl
/// detaches the `Object` from the pool, dropping the underlying client
/// and forcing a fresh physical connection on the next demand.
/// This is the only way to make `with_client` safe given Djogi's
/// `RecyclingMethod::Fast` recycler, which only checks `is_closed()` and
/// does NOT issue `ROLLBACK` / `RESET ALL` / `DISCARD ALL`. Without the
/// dirty-exit detach a closure that starts a transaction or runs `SET
/// ROLE` / `SET search_path` / an advisory lock and then errors or
/// panics would leak its session state to the next checkout — a real
/// auth/tenant/session-state hazard.
struct WithClientGuard {
    /// The checked-out object. Always `Some` until the guard is
    /// dropped — the `Option` exists only so `Drop` can move out of it
    /// and either let it drop normally (clean) or hand it to
    /// `Object::take` (detach).
    obj: Option<deadpool_postgres::Object>,
    /// Set to `true` only after the closure returns `Ok`. Defaults to
    /// `false` so every non-clean exit path detaches.
    committed: bool,
}

impl Drop for WithClientGuard {
    fn drop(&mut self) {
        if let Some(obj) = self.obj.take() {
            if self.committed {
                // Clean exit — drop the Object normally so deadpool
                // returns the connection to the pool. (The destructor
                // is `Object::drop`, which calls `pool.return_object`.)
                drop(obj);
            } else {
                // Dirty exit — detach from the pool. `Object::take`
                // consumes the Object and returns the underlying
                // ClientWrapper detached from the pool's tracker;
                // dropping it here closes the underlying
                // `tokio_postgres::Client` and the socket. The pool
                // will create a fresh physical connection on next
                // demand.
                let _client_wrapper = deadpool_postgres::Object::take(obj);
                // `_client_wrapper` drops at end of scope, closing
                // the connection.
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Builder for [`DjogiPool`].
/// Construct via [`DjogiPool::builder`]. Every field has a sensible default;
/// `.build().await` finalises into a usable pool.
pub struct DjogiPoolBuilder {
    url: String,
    max_size: usize,
    wait_timeout: Option<Duration>,
    post_connect: Option<PostConnectFn>,
}

impl DjogiPoolBuilder {
    /// Internal constructor — adopter code reaches this through
    /// [`DjogiPool::builder`].
    fn new(url: String) -> Self {
        Self {
            url,
            max_size: DEFAULT_MAX_SIZE,
            wait_timeout: None,
            post_connect: None,
        }
    }

    /// Set the maximum number of connections the pool will hold.
    /// `max_size` is the hard cap on physical connections. Once the pool
    /// has handed out `max_size` connections, additional `get` requests
    /// queue until a checkout is returned (or the wait timeout fires, if
    /// configured via [`Self::timeout`]).
    /// Default: [`DEFAULT_MAX_SIZE`].
    /// `value` must be `>= 1`. Passing `0` would build a pool whose
    /// internal semaphore has zero permits, so every `pool.get()` would
    /// block forever (or until the wait timeout fires). The check runs
    /// in [`Self::build`] so the failure surfaces at construction time
    /// with a clear error rather than as a mysterious hang at first
    /// query.
    /// Sizing guidance: pick `max_size` to match your service's expected
    /// concurrent database-touching tasks, NOT your CPU count. A web
    /// server handling 200 concurrent requests that each issue 2-3
    /// sequential queries needs roughly 30-50 connections, not 8.
    /// ```ignore
    /// let pool = djogi::pg::pool::DjogiPool::builder(&database_url)
    ///     .max_size(20)
    ///     .build()
    ///     .await?;
    /// ```
    pub fn max_size(mut self, value: usize) -> Self {
        self.max_size = value;
        self
    }

    /// Set the maximum time `pool.get` (and the implicit get inside every
    /// terminal query) will wait for a slot before returning
    /// [`DjogiError::PoolTimeout`].
    /// Default: no timeout — callers wait indefinitely. Production
    /// services typically pick a budget in the 1-10 second range so a
    /// saturated pool fails fast instead of accumulating unbounded queue
    /// depth.
    /// This sets deadpool's `wait` timeout (waiting for a free slot). The
    /// `create` and `recycle` timeouts are independent and not exposed
    /// through this builder; they default to "no timeout" and are managed
    /// by deadpool internally.
    /// ```ignore
    /// let pool = djogi::pg::pool::DjogiPool::builder(&database_url)
    ///     .max_size(20)
    ///     .timeout(std::time::Duration::from_secs(5))
    ///     .build()
    ///     .await?;
    /// ```
    pub fn timeout(mut self, value: Duration) -> Self {
        self.wait_timeout = Some(value);
        self
    }

    /// Register a closure that runs on every freshly-created physical
    /// connection.
    /// The closure receives `&mut tokio_postgres::Client` and may issue
    /// any setup SQL it needs:
    /// ```ignore
    /// use std::pin::Pin;
    ///
    /// let pool = DjogiPool::builder("postgres://localhost/app")
    ///     .post_connect(|client| Box::pin(async move {
    ///         client.batch_execute("SET statement_timeout = '5s'").await?;
    ///         client.batch_execute("SET application_name = 'web'").await?;
    ///         Ok(())
    ///     }))
    ///     .build()
    ///     .await?;
    /// ```
    /// # Lifecycle (per-physical-connection, not per-checkout)
    /// The hook fires **once per physical connection**, immediately after
    /// `Manager::create` returns a freshly-opened socket. It does NOT
    /// fire on the per-checkout path that reuses an existing physical
    /// connection. This is the correct semantic for one-time setup like
    /// `SET heer.node_id` (HeeRanjID's id-generation salt) or
    /// `SET application_name`.
    /// Lowering: the closure is wrapped into a deadpool
    /// `Hook::async_fn(...)` and attached to the pool builder via
    /// `PoolBuilder::post_create`. The deadpool docs are explicit that
    /// `post_create` runs after `Manager::create` (the physical-connection
    /// path) and is distinct from `pre_recycle` / `post_recycle`, which
    /// fire on the checkout-path validation/reuse cycle. Per-checkout
    /// reset is intentionally NOT exposed in v0.1.0 — running session-
    /// level `SET`s on every checkout would conflict with the
    /// transaction-local `set_config('app.tenant_id', $1, true)` that
    /// [`DjogiContext::set_tenant`](crate::context::DjogiContext::set_tenant)
    /// already issues at the start of every tenant-scoped transaction.
    /// # Errors
    /// If the closure returns `Err`, the underlying `Manager::create` is
    /// considered failed: deadpool discards the connection and the
    /// originating `pool.get()` (or terminal query) returns the error
    /// wrapped in [`DjogiError`]. The hook can return `Err` to abort
    /// startup when a required GUC fails to apply. The implementation
    /// prefixes hook failures with `post_connect:` so caller logs can
    /// distinguish setup failures from ordinary checkout failures.
    /// # Sharing the closure
    /// The boxed closure is kept behind an `Arc`, so a single hook is
    /// shared across all physical connections without per-create
    /// allocation. The hook bound is `Send + Sync + 'static` because
    /// deadpool's `Hook::async_fn` requires it.
    pub fn post_connect<F>(mut self, hook: F) -> Self
    where
        F: for<'a> Fn(&'a mut tokio_postgres::Client) -> PostConnectFuture<'a>
            + Send
            + Sync
            + 'static,
    {
        self.post_connect = Some(Arc::new(hook));
        self
    }

    /// Finalise the builder into a usable [`DjogiPool`].
    /// This constructs the deadpool pool eagerly — the `tokio` runtime
    /// must be available when `build` is awaited. The pool itself opens
    /// connections lazily on first checkout.
    /// ```ignore
    /// let pool = djogi::pg::pool::DjogiPool::builder(&database_url)
    ///     .max_size(10)
    ///     .timeout(std::time::Duration::from_secs(3))
    ///     .post_connect(|client| Box::pin(async move {
    ///         client.batch_execute("SET application_name = 'djogi-app'").await?;
    ///         Ok(())
    ///     }))
    ///     .build()
    ///     .await?;
    /// ```
    /// # Errors
    /// Returns [`DjogiError::Validation`] if `max_size` is `0` (a
    /// zero-permit pool would hang every `get` call forever). Returns
    /// [`DjogiError::Db`] if the underlying `deadpool` config fails to
    /// build.
    pub async fn build(self) -> Result<DjogiPool, DjogiError> {
        // Validate all registered presentation-codec startup requirements
        // (GH #227). Keyed built-ins (e.g. HmacSha256HexString) confirm their
        // env-var key is present and correctly formatted. Collect-all: all errors
        // are surfaced in a single startup message rather than one at a time.
        crate::presentation::validate_startup_inventory()
            .map_err(DjogiError::PresentationStartup)?;

        if self.max_size == 0 {
            return Err(DjogiError::Validation(
                "DjogiPoolBuilder::max_size must be >= 1; \
                 a zero-permit pool would block every checkout indefinitely"
                    .to_owned(),
            ));
        }

        let url = self.url;
        let mut cfg = Config::new();
        cfg.url = Some(url.clone());
        cfg.manager = Some(ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        });
        cfg.pool = Some(PoolConfig::new(self.max_size));

        // `cfg.builder()` returns the deadpool `PoolBuilder`. We finish it
        // off ourselves so we can attach `wait_timeout` — the high-level
        // `cfg.create_pool` does not expose it.
        let mut pool_builder = cfg.builder(NoTls).map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "DjogiPool builder: invalid config — {e}"
            )))
        })?;
        pool_builder = pool_builder.runtime(Runtime::Tokio1);

        if let Some(d) = self.wait_timeout {
            pool_builder = pool_builder.wait_timeout(Some(d));
        }

        if let Some(hook) = self.post_connect {
            // Wrap the user closure into deadpool's `Hook::async_fn` shape.
            // `Manager::Type` for `deadpool_postgres::Manager` is
            // `ClientWrapper`, which derefs to `tokio_postgres::Client`
            // so `&mut wrapper` coerces to `&mut Client` via deref-mut.
            // `Hook::async_fn` requires `Sync + Send + 'static`, hence the
            // `Arc` clone-into-the-future pattern: the outer `move`
            // closure (called by deadpool per create) clones the Arc into
            // the boxed future, then `await`s the user closure on a
            // distinct &mut borrow of the same `wrapper`.
            pool_builder = pool_builder.post_create(deadpool_postgres::Hook::async_fn(
                move |wrapper, _metrics| {
                    let hook = hook.clone();
                    Box::pin(async move {
                        let client: &mut tokio_postgres::Client = &mut *wrapper;
                        match hook(client).await {
                            Ok(()) => Ok(()),
                            Err(e) => Err(deadpool_postgres::HookError::message(format!(
                                "post_connect: {e}"
                            ))),
                        }
                    })
                },
            ));
        }

        let pool = pool_builder.build().map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "DjogiPool::build: pool creation failed: {e}"
            )))
        })?;

        Ok(DjogiPool {
            inner: pool,
            url: Some(url),
            pool_id: next_pool_id(),
        })
    }
}

impl std::fmt::Debug for DjogiPoolBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DjogiPoolBuilder")
            .field("max_size", &self.max_size)
            .field("wait_timeout", &self.wait_timeout)
            .field("post_connect", &self.post_connect.is_some())
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the effective `max_size` for a [`DjogiPool`] from a loaded
/// [`DatabaseConfig`](crate::config::DatabaseConfig).
/// Reading order (highest priority first):
/// 1. `DJOGI_DATABASE_MAX_CONNECTIONS` env var.
/// 2. `[database].max_connections` from the config.
/// 3. `None` — caller falls back to the builder default
///    ([`DEFAULT_MAX_SIZE`]).
///    Returning `None` rather than `Some(DEFAULT_MAX_SIZE)` lets the caller
///    distinguish "use whatever the builder defaults to today" from "use
///    exactly 5".
///    Zero / empty / unparseable values at any layer fall through — a typo
///    must not silently zero the pool.
pub fn resolve_max_connections(cfg: &crate::config::DatabaseConfig) -> Option<usize> {
    read_env_max_connections().or_else(|| cfg.max_connections.and_then(non_zero_size))
}

/// Treat zero as "absent". `as usize` is safe — `usize` is ≥ 32 bits on
/// every platform Djogi targets.
fn non_zero_size(n: u32) -> Option<usize> {
    if n > 0 { Some(n as usize) } else { None }
}

/// Read [`ENV_DATABASE_MAX_CONNECTIONS`] and parse it as a positive `usize`.
/// Empty / unparseable / `0` values fall through; the parse uses `usize` so
/// values up to `usize::MAX` are honoured rather than narrowed at `u32::MAX`.
fn read_env_max_connections() -> Option<usize> {
    let n = std::env::var(ENV_DATABASE_MAX_CONNECTIONS)
        .ok()?
        .trim()
        .parse::<usize>()
        .ok()?;
    if n == 0 { None } else { Some(n) }
}

/// Lower a `deadpool_postgres::PoolError` into a `DjogiError`, mapping
/// the timeout variants to [`DjogiError::PoolTimeout`] so callers can
/// match on them without inspecting the deadpool error type.
pub(crate) fn map_pool_err(e: deadpool_postgres::PoolError) -> DjogiError {
    use deadpool_postgres::PoolError as P;
    match e {
        P::Timeout(deadpool_postgres::TimeoutType::Wait) => {
            DjogiError::PoolTimeout { phase: "wait" }
        }
        P::Timeout(deadpool_postgres::TimeoutType::Create) => {
            DjogiError::PoolTimeout { phase: "create" }
        }
        P::Timeout(deadpool_postgres::TimeoutType::Recycle) => {
            DjogiError::PoolTimeout { phase: "recycle" }
        }
        other => {
            tracing::error!("DjogiPool: deadpool error: {other}");
            DjogiError::Db(DbError::other(format!("pool error: {other}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construction-only smoke test — the builder builds a pool against an
    /// obviously-bogus URL because deadpool defers physical connection
    /// until the first checkout. The test asserts the builder API itself
    /// does not panic and that `Debug` works.
    #[tokio::test]
    async fn builder_constructs_pool_with_defaults() {
        let pool = DjogiPool::builder("postgres://localhost/_djogi_unreachable")
            .build()
            .await
            .expect("builder should construct pool eagerly without connecting");
        let _ = format!("{pool:?}");
    }

    /// `connect(url)` keeps the pre-builder shape — same defaults, same
    /// return type, same async signature.
    #[tokio::test]
    async fn connect_delegates_to_builder() {
        let _pool = DjogiPool::connect("postgres://localhost/_djogi_unreachable")
            .await
            .expect("connect should construct pool with builder defaults");
    }

    /// `max_size` and `timeout` are accepted without runtime panic; the
    /// resulting pool's debug output reflects the configured cap.
    #[tokio::test]
    async fn builder_accepts_max_size_and_timeout() {
        let pool = DjogiPool::builder("postgres://localhost/_djogi_unreachable")
            .max_size(7)
            .timeout(Duration::from_millis(50))
            .build()
            .await
            .expect("builder should accept max_size and timeout");
        let dbg = format!("{pool:?}");
        // `Pool::status()` exposes `max_size` — the Debug impl includes it.
        assert!(
            dbg.contains("max_size: 7"),
            "Debug output should reflect max_size, got: {dbg}"
        );
    }

    /// A zero `max_size` would build a pool whose semaphore has no
    /// permits, hanging every checkout forever. Reject at `build` time.
    #[tokio::test]
    async fn builder_rejects_zero_max_size() {
        let err = DjogiPool::builder("postgres://localhost/_djogi_unreachable")
            .max_size(0)
            .build()
            .await
            .expect_err("max_size(0) must be rejected");
        match err {
            DjogiError::Validation(msg) => {
                assert!(
                    msg.contains("max_size") && msg.contains(">= 1"),
                    "Validation message should mention max_size and >= 1; got: {msg}"
                );
            }
            other => panic!("expected DjogiError::Validation, got: {other:?}"),
        }
    }

    /// `map_pool_err` lowers each deadpool timeout variant into the
    /// matching `DjogiError::PoolTimeout { phase }`.
    #[test]
    fn map_pool_err_lowers_timeouts() {
        use deadpool_postgres::{PoolError, TimeoutType};

        match map_pool_err(PoolError::Timeout(TimeoutType::Wait)) {
            DjogiError::PoolTimeout { phase } => assert_eq!(phase, "wait"),
            other => panic!("expected PoolTimeout(wait), got: {other:?}"),
        }
        match map_pool_err(PoolError::Timeout(TimeoutType::Create)) {
            DjogiError::PoolTimeout { phase } => assert_eq!(phase, "create"),
            other => panic!("expected PoolTimeout(create), got: {other:?}"),
        }
        match map_pool_err(PoolError::Timeout(TimeoutType::Recycle)) {
            DjogiError::PoolTimeout { phase } => assert_eq!(phase, "recycle"),
            other => panic!("expected PoolTimeout(recycle), got: {other:?}"),
        }
    }

    /// PoolTimeout is classified as transient — generic retry helpers
    /// must treat it as a back-off-and-retry condition, not a permanent
    /// failure.
    #[test]
    fn pool_timeout_is_transient() {
        let err = DjogiError::PoolTimeout { phase: "wait" };
        assert!(err.is_transient(), "PoolTimeout must be transient");
        assert!(!err.is_terminal(), "PoolTimeout must NOT be terminal");
    }

    /// `resolve_max_connections` walks env > config > None in the
    /// documented order and rejects degenerate env values without
    /// zeroing the pool. The lib test sweep runs single-threaded, so
    /// the env-var mutation is safe — the test resets the variable
    /// after every assertion.
    #[test]
    fn resolve_max_connections_walks_env_then_config() {
        use crate::config::DatabaseConfig;

        // Helper: clear the env var before each branch so previous
        // sub-cases don't leak in.
        let clear_env = || {
            // Safety: process-global mutation; the test runs
            // single-threaded under the project's `--test-threads=1`
            // policy and resets the variable before each branch.
            unsafe { std::env::remove_var(ENV_DATABASE_MAX_CONNECTIONS) };
        };

        // Layer 3: no env, config zero → None (fall back to default).
        clear_env();
        let cfg = DatabaseConfig {
            url: String::new(),
            crud_log_url: None,
            event_log_url: None,
            max_connections: None,
            dev_mode: false,
        };
        assert_eq!(resolve_max_connections(&cfg), None);

        // Layer 2: no env, config non-zero → Some(config).
        clear_env();
        let cfg = DatabaseConfig {
            url: String::new(),
            crud_log_url: None,
            event_log_url: None,
            max_connections: Some(25),
            dev_mode: false,
        };
        assert_eq!(resolve_max_connections(&cfg), Some(25));

        // Layer 1: env wins over config.
        unsafe { std::env::set_var(ENV_DATABASE_MAX_CONNECTIONS, "42") };
        let cfg = DatabaseConfig {
            url: String::new(),
            crud_log_url: None,
            event_log_url: None,
            max_connections: Some(25),
            dev_mode: false,
        };
        assert_eq!(resolve_max_connections(&cfg), Some(42));

        // Layer 1 → 2 fall-through: empty env value → use config.
        unsafe { std::env::set_var(ENV_DATABASE_MAX_CONNECTIONS, "  ") };
        let cfg = DatabaseConfig {
            url: String::new(),
            crud_log_url: None,
            event_log_url: None,
            max_connections: Some(25),
            dev_mode: false,
        };
        assert_eq!(resolve_max_connections(&cfg), Some(25));

        // Layer 1 → 2 fall-through: bogus env value → use config.
        unsafe { std::env::set_var(ENV_DATABASE_MAX_CONNECTIONS, "not-a-number") };
        let cfg = DatabaseConfig {
            url: String::new(),
            crud_log_url: None,
            event_log_url: None,
            max_connections: Some(25),
            dev_mode: false,
        };
        assert_eq!(resolve_max_connections(&cfg), Some(25));

        // Layer 1 → 2 fall-through: zero env → use config (never zero
        // the pool from a typoed env var).
        unsafe { std::env::set_var(ENV_DATABASE_MAX_CONNECTIONS, "0") };
        let cfg = DatabaseConfig {
            url: String::new(),
            crud_log_url: None,
            event_log_url: None,
            max_connections: Some(25),
            dev_mode: false,
        };
        assert_eq!(resolve_max_connections(&cfg), Some(25));

        // Layer 1 → 2 → 3 fall-through: zero env, zero config → None.
        unsafe { std::env::set_var(ENV_DATABASE_MAX_CONNECTIONS, "0") };
        let cfg = DatabaseConfig {
            url: String::new(),
            crud_log_url: None,
            event_log_url: None,
            max_connections: None,
            dev_mode: false,
        };
        assert_eq!(resolve_max_connections(&cfg), None);

        clear_env();
    }

    /// `from_database_config` smoke-test: builds a pool against an
    /// obviously-bogus URL with config-driven max_size and asserts the
    /// max_size flows through to the pool's status output.
    #[tokio::test]
    async fn from_database_config_applies_max_size() {
        use crate::config::DatabaseConfig;

        // Make sure the env doesn't bleed in from another test.
        unsafe { std::env::remove_var(ENV_DATABASE_MAX_CONNECTIONS) };

        let cfg = DatabaseConfig {
            url: "postgres://localhost/_djogi_unreachable".to_string(),
            crud_log_url: None,
            event_log_url: None,
            max_connections: Some(11),
            dev_mode: false,
        };
        let pool = DjogiPool::from_database_config(&cfg)
            .await
            .expect("from_database_config should construct pool");
        let dbg = format!("{pool:?}");
        assert!(
            dbg.contains("max_size: 11"),
            "Debug output should reflect config max_size, got: {dbg}"
        );
    }

    /// `with_client` accepts the documented closure shape. Live
    /// execution (closure body actually running against a checked-out
    /// connection) requires a reachable Postgres and lives in the
    /// integration tests (`pool_with_client_returns_connection_on_ok` /
    /// `pool_with_client_detaches_on_panic`).
    #[allow(dead_code)] // type-check only — body never runs
    fn with_client_accepts_documented_closure_shape() {
        fn _check(
            pool: &DjogiPool,
        ) -> impl std::future::Future<Output = Result<i32, DjogiError>> + '_ {
            pool.with_client(|client| {
                Box::pin(async move {
                    // Body never runs in this static type-check;
                    // suppress the unused-variable warning by binding
                    // and dropping. The compile-time assertion is that
                    // a `&mut tokio_postgres::Client` body returning
                    // `Result<i32, DjogiError>` satisfies the bound.
                    let _ = client;
                    Ok(42)
                })
            })
        }
    }

    /// The builder accepts a `post_connect` closure with the documented
    /// shape and surfaces its presence in `Debug` output. The hook only
    /// fires when deadpool calls `Manager::create`, which requires a
    /// reachable Postgres — the live invocation-count assertion lives in
    /// `pool_post_connect_fires_once_per_physical_connection`.
    #[tokio::test]
    async fn builder_accepts_post_connect_closure() {
        let pool = DjogiPool::builder("postgres://localhost/_djogi_unreachable")
            .post_connect(|client| {
                Box::pin(async move {
                    // Closure body never runs in this unit test — no
                    // physical connection is opened. We just type-check
                    // the closure signature.
                    let _ = client;
                    Ok(())
                })
            })
            .build()
            .await
            .expect("builder should accept post_connect closure");
        // Build succeeded — pool is unused but its presence pins the
        // type-system shape against future drift.
        let _ = pool;
    }
}
