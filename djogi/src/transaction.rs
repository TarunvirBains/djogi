//! `atomic(...)` — the canonical transaction scope + retry helper.
//! # `atomic()` at a glance
//! `atomic(&mut ctx, |tx| Box::pin(async move { ... }))` is the preferred
//! outermost entry point when the caller already has a pool-backed
//! [`DjogiContext`](crate::DjogiContext): it acquires a connection from the
//! context's pool, issues `BEGIN`, wraps the connection in a transaction
//! context that shares the parent context's `Arc<Sassi>`, runs the closure,
//! commits on `Ok`, rolls back on `Err`, and drains the on-commit callback
//! queue after a successful commit. `atomic(&pool, |tx| ...)` remains the
//! compatibility shortcut when no parent context exists; it constructs a fresh
//! top-level context for that transaction. Nested calls
//! `atomic(&mut *outer, |inner| Box::pin(async move { ... }))` — push a
//! Postgres savepoint rather than opening a new transaction: the inner scope
//! rolls back to / releases the savepoint on `Err`/`Ok` respectively, and on
//! success promotes its on-commit callbacks to the outer context so they drain
//! once at the outermost commit.
//! # Isolation level
//! [`atomic_with`] is the sibling helper that opens the outermost transaction
//! at an explicit Postgres isolation level via `BEGIN ISOLATION LEVEL <level>`.
//! See the [`IsolationLevel`] enum docs for the variant matrix. The nested
//! savepoint path explicitly rejects an isolation-level argument because
//! Postgres pins the isolation level for the entire transaction at the outer
//! `BEGIN` — `SAVEPOINT` does not open a sub-transaction with its own
//! isolation knob.
//! # Panic semantics
//! Closure panics never leak an uncommitted transaction. The closure
//! future is wrapped in
//! [`AssertUnwindSafe`](std::panic::AssertUnwindSafe) and polled through
//! [`FutureExt::catch_unwind`](futures::FutureExt::catch_unwind); on
//! the panic branch we issue the appropriate rollback (outer
//! `ROLLBACK` or `ROLLBACK TO SAVEPOINT`) **before** the
//! panic resumes. `AssertUnwindSafe` is the conventional escape hatch
//! for async boundaries — user-supplied closures rarely implement
//! `UnwindSafe`, and the context state touched inside the closure is
//! owned for the closure's lifetime alone.
//! # Retry helper
//! [`retry_on_conflict`] composes with `atomic()` (and `atomic_with()`)
//! to re-run a closure on serialization / deadlock / `NOWAIT` failures.
//! `IsolationLevel::Serializable` / `IsolationLevel::RepeatableRead` raise
//! SQLSTATE `40001` (serialization failure) on commit-time conflict;
//! [`crate::DjogiError::is_transient`] classifies that as retryable, so the
//! retry loop drives the typed isolation surface without extra wiring. Pure
//! retry — no backoff — remains available via [`retry_on_conflict`]. Use
//! [`retry_on_conflict_with_backoff`] when the closure can fail during pool
//! saturation and immediate retries would amplify checkout pressure.

use crate::context::{
    AuthStateSnapshot, ContextInner, DjogiContext, NESTED_ATOMIC_CANCELLED_POISON_REASON,
};
use crate::pg::connection::PgConnection;
use crate::pg::pool::DjogiPool;
use crate::{DbError, DjogiError};
use futures::FutureExt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, resume_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static JITTER_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Boxed future tied to the caller's context-reborrow lifetime.
/// Every `atomic()` closure returns one of these. The `'a` lifetime
/// ties the future's body to the `&'a mut DjogiContext` the closure
/// receives, so the closure can freely `.await` framework calls that
/// also borrow from the context. The `Pin<Box<..>>` erasure is what
/// lets the outer `atomic()` signature use a `for<'a>` higher-ranked
/// bound without falling into the "async closure implementation not
/// general enough" inference hole that bare `AsyncFnOnce` hits today.
pub type AtomicFuture<'a, R> = Pin<Box<dyn Future<Output = Result<R, DjogiError>> + Send + 'a>>;

enum TopLevelAtomicOwner {
    Connection(PgConnection),
    Context(DjogiContext),
}

impl TopLevelAtomicOwner {
    fn conn_mut(&mut self) -> &mut PgConnection {
        match self {
            TopLevelAtomicOwner::Connection(conn) => conn,
            TopLevelAtomicOwner::Context(ctx) => match ctx.inner_mut() {
                ContextInner::Transaction(conn) => conn,
                ContextInner::Pool(_) => {
                    unreachable!("top-level atomic owner should never hold a pool-backed context",)
                }
            },
        }
    }

    fn detach(self) {
        match self {
            TopLevelAtomicOwner::Connection(conn) => conn.detach(),
            TopLevelAtomicOwner::Context(ctx) => ctx.detach_transaction_connection(),
        }
    }
}

struct TopLevelAtomicGuard {
    owner: Option<TopLevelAtomicOwner>,
    clean: bool,
    scope: &'static str,
}

impl TopLevelAtomicGuard {
    fn from_connection(conn: PgConnection, scope: &'static str) -> Self {
        Self {
            owner: Some(TopLevelAtomicOwner::Connection(conn)),
            clean: false,
            scope,
        }
    }

    fn conn_mut(&mut self) -> &mut PgConnection {
        self.owner
            .as_mut()
            .expect("top-level atomic guard owns the connection until Drop")
            .conn_mut()
    }

    fn promote_to_context<F>(&mut self, build_ctx: F)
    where
        F: FnOnce(PgConnection) -> DjogiContext,
    {
        let owner = self
            .owner
            .take()
            .expect("promote_to_context requires an owned connection");
        let conn = match owner {
            TopLevelAtomicOwner::Connection(conn) => conn,
            TopLevelAtomicOwner::Context(_) => {
                unreachable!("top-level atomic owner already promoted to DjogiContext")
            }
        };
        self.owner = Some(TopLevelAtomicOwner::Context(build_ctx(conn)));
    }

    fn tx_ctx_mut(&mut self) -> &mut DjogiContext {
        match self
            .owner
            .as_mut()
            .expect("top-level atomic guard owns the transaction context until Drop")
        {
            TopLevelAtomicOwner::Connection(_) => {
                unreachable!("transaction context requested before BEGIN completed")
            }
            TopLevelAtomicOwner::Context(ctx) => ctx,
        }
    }

    async fn commit(&mut self) -> Result<(), DjogiError> {
        let result = self.tx_ctx_mut().commit_in_place().await;
        if result.is_ok()
            || (matches!(&result, Err(DjogiError::TransactionPoisoned { .. }))
                && !self.tx_ctx_mut().is_transaction_poisoned())
        {
            self.clean = true;
        }
        result
    }

    async fn rollback(&mut self) -> Result<(), DjogiError> {
        self.tx_ctx_mut().rollback_in_place().await?;
        self.clean = true;
        Ok(())
    }
}

impl Drop for TopLevelAtomicGuard {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.take() {
            if self.clean {
                drop(owner);
            } else {
                tracing::warn!(
                    scope = self.scope,
                    "djogi::transaction::atomic::dirty_detach: detaching dirty \
                     top-level transaction connection on drop",
                );
                owner.detach();
            }
        }
    }
}

struct PoolContextAtomicGuard<'a> {
    parent_ctx: &'a mut DjogiContext,
    inner: TopLevelAtomicGuard,
}

impl<'a> PoolContextAtomicGuard<'a> {
    fn new(parent_ctx: &'a mut DjogiContext, conn: PgConnection) -> Self {
        Self {
            parent_ctx,
            inner: TopLevelAtomicGuard::from_connection(conn, "pool_ctx"),
        }
    }

    fn conn_mut(&mut self) -> &mut PgConnection {
        self.inner.conn_mut()
    }

    fn promote_to_context(&mut self) {
        let sassi = std::sync::Arc::clone(&self.parent_ctx.sassi);
        let auth = self.parent_ctx.auth.clone();
        let tenant_scope_suppressed = self.parent_ctx.tenant_scope_suppressed;
        self.inner.promote_to_context(|conn| {
            let mut tx_ctx = DjogiContext::from_connection_with_sassi(conn, sassi);
            tx_ctx.auth = auth;
            tx_ctx.tenant_scope_suppressed = tenant_scope_suppressed;
            tx_ctx
        });
    }

    fn tx_ctx_mut(&mut self) -> &mut DjogiContext {
        self.inner.tx_ctx_mut()
    }

    async fn commit(&mut self) -> Result<(), DjogiError> {
        self.inner.commit().await
    }

    async fn rollback(&mut self) -> Result<(), DjogiError> {
        self.inner.rollback().await
    }

    fn propagate_success_to_parent(&mut self) {
        let auth_after = self.tx_ctx_mut().auth.clone();
        let tenant_scope_suppressed_after = self.tx_ctx_mut().tenant_scope_suppressed;
        self.parent_ctx.auth = auth_after;
        self.parent_ctx.tenant_scope_suppressed = tenant_scope_suppressed_after;
        clear_pool_context_transaction_trackers(self.parent_ctx);
    }

    fn clear_parent_trackers(&mut self) {
        clear_pool_context_transaction_trackers(self.parent_ctx);
    }
}

impl Drop for PoolContextAtomicGuard<'_> {
    fn drop(&mut self) {
        if !self.inner.clean {
            clear_pool_context_transaction_trackers(self.parent_ctx);
        }
    }
}

// ---------------------------------------------------------------------------
// Isolation level — typed surface for `BEGIN ISOLATION LEVEL <level>`.
// ---------------------------------------------------------------------------

/// Postgres transaction isolation level.
/// Maps 1:1 to the SQL standard's three isolation levels Postgres
/// actually distinguishes. (Postgres also accepts `READ UNCOMMITTED`
/// but aliases it to `READ COMMITTED` — it provides no weaker
/// guarantees than `READ COMMITTED` on Postgres so the enum does not
/// expose that variant.)
/// Used by [`atomic_with`] to open the outermost transaction at an
/// explicit isolation level via `BEGIN ISOLATION LEVEL <level>`. The
/// level applies to the entire transaction — once the outer `BEGIN`
/// fixes the isolation, Postgres pins it for the duration; `SAVEPOINT`
/// does not open a sub-transaction with its own isolation knob, so
/// nested `atomic_with` calls are rejected (see [`atomic_with`] docs).
/// # Variants
/// - [`IsolationLevel::ReadCommitted`] — Postgres' session default. A
///   statement sees only data committed before the statement begins
///   (snapshot of the moment the statement starts). Different
///   statements in one transaction can observe different commits. The
///   weakest isolation Postgres provides; widest concurrency.
/// - [`IsolationLevel::RepeatableRead`] — every statement in the
///   transaction sees the same snapshot, taken at the moment of the
///   transaction's first non-control statement. Reads are repeatable;
///   concurrent writes that conflict raise SQLSTATE `40001`
///   (serialization_failure) at commit time. [`retry_on_conflict`]
///   classifies that as transient and re-runs the closure.
/// - [`IsolationLevel::Serializable`] — strongest. Postgres' SSI
///   (serializable snapshot isolation) monitors read/write dependencies
///   between concurrent transactions and aborts one with `40001` if
///   their interleaving could not be reproduced by some serial
///   execution. Use for invariants that span multiple rows or tables
///   (e.g. "no two events can overlap in the same room").
/// # Retry composition
/// Both `RepeatableRead` and `Serializable` can raise serialization
/// failure on commit. [`retry_on_conflict`] composes naturally — wrap
/// the `atomic_with` call in `retry_on_conflict` to re-run on `40001`:
/// ```ignore
/// retry_on_conflict(&mut ctx, 3, async |ctx| {
/// atomic_with(IsolationLevel::Serializable, ctx, |tx| Box::pin(async move {
/// // ... reads + writes ...
/// Ok::<_, DjogiError>(())
/// })).await
/// }).await?;
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IsolationLevel {
    /// `READ COMMITTED` — Postgres' session default. See enum docs.
    ReadCommitted,
    /// `REPEATABLE READ` — snapshot fixed at first non-control
    /// statement; serialization-failure on commit conflict. See enum
    /// docs.
    RepeatableRead,
    /// `SERIALIZABLE` — Postgres' strongest isolation via SSI.
    /// Aborts conflicting transactions with `40001`. See enum docs.
    Serializable,
}

impl IsolationLevel {
    /// SQL keyword for this isolation level, matching Postgres' SQL
    /// grammar exactly. Used by [`atomic_with`] to compose the
    /// `BEGIN ISOLATION LEVEL <keyword>` statement.
    /// The return is `&'static str` — all three keywords are
    /// compile-time literals, no allocation.
    pub const fn as_sql_keyword(self) -> &'static str {
        match self {
            IsolationLevel::ReadCommitted => "READ COMMITTED",
            IsolationLevel::RepeatableRead => "REPEATABLE READ",
            IsolationLevel::Serializable => "SERIALIZABLE",
        }
    }
}

impl std::fmt::Display for IsolationLevel {
    /// Delegate to [`Self::as_sql_keyword`] so `{level}` in error
    /// messages and traces reads as the SQL keyword.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_sql_keyword())
    }
}

/// Compose the `BEGIN ISOLATION LEVEL <level>` statement for `level`.
/// Inlined into both pool-path entry points (pool reference, pool-
/// backed context) so the SQL composition lives in one place. Uses
/// [`IsolationLevel::as_sql_keyword`] for the literal — no user
/// input flows into the SQL.
fn begin_with_isolation_sql(level: IsolationLevel) -> String {
    format!("BEGIN ISOLATION LEVEL {}", level.as_sql_keyword())
}

// ---------------------------------------------------------------------------
// Retry backoff policy — dependency-free helper for saturated-pool retries.
// ---------------------------------------------------------------------------

/// Retryable error-class selector for [`TransactionRetryBackoff`].
/// Defaults to djogi's current transient classes:
/// [`DjogiError::LockConflict`], `Db(40001|40P01|55P03)`, and
/// [`DjogiError::PoolTimeout`]. Callers can disable any class to make retry
/// behavior explicit rather than incidental.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryableErrorClasses {
    lock_conflict: bool,
    db_lock_conflict: bool,
    pool_timeout: bool,
}

impl Default for RetryableErrorClasses {
    fn default() -> Self {
        Self {
            lock_conflict: true,
            db_lock_conflict: true,
            pool_timeout: true,
        }
    }
}

impl RetryableErrorClasses {
    /// Construct the default class set (`LockConflict`, lock SQLSTATEs, and
    /// `PoolTimeout`).
    pub fn all() -> Self {
        Self::default()
    }

    /// Enable or disable retries for [`DjogiError::LockConflict`].
    pub fn with_lock_conflict(mut self, enabled: bool) -> Self {
        self.lock_conflict = enabled;
        self
    }

    /// Enable or disable retries for lock SQLSTATEs carried via
    /// [`DjogiError::Db`] (`40001`, `40P01`, `55P03`).
    pub fn with_db_lock_conflict(mut self, enabled: bool) -> Self {
        self.db_lock_conflict = enabled;
        self
    }

    /// Enable or disable retries for [`DjogiError::PoolTimeout`].
    pub fn with_pool_timeout(mut self, enabled: bool) -> Self {
        self.pool_timeout = enabled;
        self
    }

    fn is_retryable(self, error: &DjogiError) -> bool {
        match error {
            DjogiError::LockConflict(_) => self.lock_conflict,
            DjogiError::Db(db_error) => {
                self.db_lock_conflict && crate::error::is_lock_error(db_error)
            }
            DjogiError::PoolTimeout { .. } => self.pool_timeout,
            _ => false,
        }
    }
}

/// Backoff policy for [`retry_on_conflict_with_backoff`].
/// The policy is deliberately dependency-free. Known-primitive audit:
/// - Sleeping uses [`tokio::time::sleep`], already part of Djogi's async
///   runtime substrate.
/// - Delay arithmetic uses [`std::time::Duration`] and saturating operations.
/// - Optional jitter uses [`std::time::SystemTime`] as a lightweight entropy
///   source rather than adding a `rand` dependency.
///   The default policy treats [`DjogiError::PoolTimeout`] as a stronger pressure
///   signal than lock conflicts: pool checkout exhaustion waits longer before
///   retrying so callers do not immediately re-enter the saturated queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionRetryBackoff {
    lock_conflict_initial_delay: Duration,
    pool_timeout_initial_delay: Duration,
    max_delay: Duration,
    jitter: Duration,
    retryable_error_classes: RetryableErrorClasses,
}

impl Default for TransactionRetryBackoff {
    fn default() -> Self {
        Self {
            lock_conflict_initial_delay: Duration::from_millis(5),
            pool_timeout_initial_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(1),
            jitter: Duration::from_millis(10),
            retryable_error_classes: RetryableErrorClasses::default(),
        }
    }
}

impl TransactionRetryBackoff {
    /// Construct the default policy.
    pub fn new() -> Self {
        Self::default()
    }

    /// Disable all sleeping. Useful for tests or callers that only want the
    /// sibling helper's error-classification surface.
    pub fn none() -> Self {
        Self {
            lock_conflict_initial_delay: Duration::ZERO,
            pool_timeout_initial_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            jitter: Duration::ZERO,
            retryable_error_classes: RetryableErrorClasses::default(),
        }
    }

    /// Set the initial delay for lock / serialization conflicts.
    pub fn with_lock_conflict_initial_delay(mut self, delay: Duration) -> Self {
        self.lock_conflict_initial_delay = delay;
        self
    }

    /// Set the initial delay for pool checkout timeouts.
    pub fn with_pool_timeout_initial_delay(mut self, delay: Duration) -> Self {
        self.pool_timeout_initial_delay = delay;
        self
    }

    /// Set the maximum exponential-backoff delay before jitter is applied.
    pub fn with_max_delay(mut self, delay: Duration) -> Self {
        self.max_delay = delay;
        self
    }

    /// Set the maximum additive jitter applied by the retrying helper.
    /// Jitter is applied after the exponential delay is capped by
    /// [`Self::with_max_delay`]. Set this to [`Duration::ZERO`] for fully
    /// deterministic sleeps. Non-zero jitter is sampled across the full
    /// configured duration; values above one second are not truncated to the
    /// subsecond portion of the system clock.
    pub fn with_jitter(mut self, jitter: Duration) -> Self {
        self.jitter = jitter;
        self
    }

    /// Set which error classes [`retry_on_conflict_with_backoff`] is allowed
    /// to retry.
    pub fn with_retryable_error_classes(mut self, classes: RetryableErrorClasses) -> Self {
        self.retryable_error_classes = classes;
        self
    }

    /// Return the configured retryable class set.
    pub fn retryable_error_classes(self) -> RetryableErrorClasses {
        self.retryable_error_classes
    }

    /// Return the capped exponential delay before jitter for a failed attempt.
    /// `completed_attempt` is the one-based attempt number that just failed:
    /// the first failure gets the initial delay, the second failure doubles it,
    /// and so on until [`Self::with_max_delay`] caps it.
    pub fn base_delay_for_retry(
        self,
        error: &DjogiError,
        completed_attempt: u32,
    ) -> Option<Duration> {
        if !self.should_retry(error) {
            return None;
        }

        let base = match error {
            DjogiError::PoolTimeout { .. } => self.pool_timeout_initial_delay,
            _ => self.lock_conflict_initial_delay,
        };
        let exponent = completed_attempt.saturating_sub(1).min(31);
        let factor = 1_u32.checked_shl(exponent).unwrap_or(u32::MAX);
        Some(base.saturating_mul(factor).min(self.max_delay))
    }

    fn delay_for_retry(self, error: &DjogiError, completed_attempt: u32) -> Option<Duration> {
        let base = self.base_delay_for_retry(error, completed_attempt)?;
        if self.jitter.is_zero() {
            return Some(base);
        }

        let jitter = jitter_duration(self.jitter);
        Some(base.saturating_add(jitter))
    }

    fn should_retry(self, error: &DjogiError) -> bool {
        self.retryable_error_classes.is_retryable(error)
    }
}

fn jitter_duration(max_jitter: Duration) -> Duration {
    let max_nanos = max_jitter.as_nanos();
    if max_nanos == 0 {
        return Duration::ZERO;
    }

    nanos_to_duration(mixed_jitter_nanos(jitter_seed(), max_nanos))
}

fn jitter_seed() -> u128 {
    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let counter = JITTER_COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    now_nanos ^ ((counter as u128) << 64) ^ counter as u128
}

fn mixed_jitter_nanos(seed: u128, max_nanos: u128) -> u128 {
    if max_nanos == 0 {
        return 0;
    }

    let lo = splitmix64(seed as u64);
    let hi = splitmix64((seed >> 64) as u64 ^ 0xD1B5_4A32_D192_ED03);
    let mixed = ((hi as u128) << 64) | lo as u128;
    mixed % (max_nanos + 1)
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

fn nanos_to_duration(nanos: u128) -> Duration {
    const NANOS_PER_SEC: u128 = 1_000_000_000;
    Duration::new(
        (nanos / NANOS_PER_SEC) as u64,
        (nanos % NANOS_PER_SEC) as u32,
    )
}

// ---------------------------------------------------------------------------
// DeferScope — typed surface for SET CONSTRAINTS ALL/<names> DEFERRED/IMMEDIATE.
// ---------------------------------------------------------------------------

/// Scope payload for [`DjogiContext::defer_constraints`] /
/// [`DjogiContext::set_constraints_immediate`].
/// Mirrors the Postgres `SET CONSTRAINTS { ALL | <name> [, ...] } { DEFERRED |
/// IMMEDIATE }` grammar. Both helpers are transaction-scoped: outside an
/// open `atomic()` they raise
/// [`DjogiError::ConstraintModeOutsideTransaction`].
/// # Variants
/// - [`DeferScope::All`] — apply the new mode to every deferrable
///   constraint in the current transaction. Emits
///   `SET CONSTRAINTS ALL DEFERRED|IMMEDIATE`. Postgres' standard
///   shape for the cycle-FK insertion pattern.
/// - [`DeferScope::Named`] — apply the new mode to specific named
///   constraints. The framework validates each name against the
///   model-descriptor inventory ([`crate::DeferrabilitySpec`])
///   before emitting any SQL:
/// - Unknown name → [`DjogiError::UnknownConstraintName`].
/// - Name found but `deferrable = false` →
///   [`DjogiError::ConstraintNotDeferrable`].
/// - All names valid + deferrable → emit
///   `SET CONSTRAINTS "name1", "name2", ... DEFERRED|IMMEDIATE`.
///   The constraint naming convention is the conventional
///   `<table>_<column>_fkey` (truncated to Postgres' 63-byte
///   identifier limit when necessary) — see
///   `djogi/src/migrate/sql.rs::fk_constraint_name` for the canonical
///   composition.
/// # Slice form for `Named`
/// `Named(&'static [&'static str])` accepts a slice of static names so
/// the typical adopter call site is allocation-free:
/// ```ignore
/// ctx.defer_constraints(DeferScope::Named(&["posts_author_id_fkey"])).await?;
/// ```
/// `'static` keeps the API simple — constraint names in adopter code
/// are typically string literals known at compile time. Dynamic name
/// composition (rare) can use `Box::leak` or a `&'static str` interner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeferScope {
    /// Apply the mode change to every deferrable constraint in the
    /// transaction. Emits `SET CONSTRAINTS ALL DEFERRED|IMMEDIATE`.
    All,

    /// Apply the mode change to the named constraints. Each name is
    /// validated against the framework's `DeferrabilitySpec`
    /// inventory before any SQL is emitted.
    Named(&'static [&'static str]),
}

/// Constraint-mode keyword used by [`DjogiContext::defer_constraints`]
/// and [`DjogiContext::set_constraints_immediate`] to drive the
/// underlying `SET CONSTRAINTS ... DEFERRED|IMMEDIATE` SQL.
/// Private to the crate — the two public entry points wrap this
/// internally so adopters never name the mode directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConstraintMode {
    Deferred,
    Immediate,
}

impl ConstraintMode {
    /// SQL keyword for this mode, used in `SET CONSTRAINTS ...
    /// <keyword>` emission.
    pub(crate) const fn as_sql_keyword(self) -> &'static str {
        match self {
            ConstraintMode::Deferred => "DEFERRED",
            ConstraintMode::Immediate => "IMMEDIATE",
        }
    }
}

// ---------------------------------------------------------------------------
// Sealed trait — `&DjogiPool` and `&mut DjogiContext` are the only scopes.
// ---------------------------------------------------------------------------

mod sealed {
    pub trait Sealed {}
    impl Sealed for &crate::pg::pool::DjogiPool {}
    impl Sealed for &mut crate::DjogiContext {}
}

/// Entry point for [`atomic()`] / [`atomic_with()`].
/// Sealed: the only scopes that can open an `atomic()` block are a
/// pool reference (fresh outermost context) and a mutable [`DjogiContext`]
/// reference. A pool-backed `DjogiContext` opens an outermost transaction that
/// shares the context's `Arc<Sassi>`; a transaction-backed context opens a
/// nested savepoint.
/// The trait carries the dispatch logic through an associated
/// [`run_atomic`](IntoAtomicScope::run_atomic) method so `atomic()`
/// itself stays as a thin forwarder over the two impls. Each impl
/// owns its own commit / rollback / callback-promotion semantics.
/// The companion [`run_atomic_with`](IntoAtomicScope::run_atomic_with)
/// method threads an explicit [`IsolationLevel`] through to the outer
/// `BEGIN`. The pool-path impls compose `BEGIN ISOLATION LEVEL
/// <level>` for the open; the nested-savepoint impl rejects with
/// [`DjogiError::IsolationLevelOnNestedScope`] because Postgres pins
/// isolation at the outer `BEGIN` — savepoints cannot change it.
#[doc(hidden)]
pub trait IntoAtomicScope: sealed::Sealed {
    /// Run `closure` inside this scope. Implementations handle
    /// outermost transaction open + commit/rollback + callback drain
    /// (pool path) or savepoint push + release/rollback-to + callback
    /// promotion (nested path).
    fn run_atomic<F, R>(
        self,
        closure: F,
    ) -> impl std::future::Future<Output = Result<R, DjogiError>> + Send
    where
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send;

    /// Run `closure` inside this scope, threading an explicit Postgres
    /// isolation level through to the outer `BEGIN`.
    /// Pool-path impls emit `BEGIN ISOLATION LEVEL <level>` instead of
    /// the plain `BEGIN` of [`run_atomic`]. The nested-savepoint impl
    /// returns [`DjogiError::IsolationLevelOnNestedScope`] because
    /// Postgres pins isolation at the outer `BEGIN` for the entire
    /// transaction; `SAVEPOINT` does not open a sub-transaction with
    /// its own isolation knob.
    fn run_atomic_with<F, R>(
        self,
        level: IsolationLevel,
        closure: F,
    ) -> impl std::future::Future<Output = Result<R, DjogiError>> + Send
    where
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send;
}

// ---------------------------------------------------------------------------
// Pool-backed impl — the outermost entry point.
// ---------------------------------------------------------------------------

impl IntoAtomicScope for &DjogiPool {
    async fn run_atomic<F, R>(self, closure: F) -> Result<R, DjogiError>
    where
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send,
    {
        run_pool_atomic_inner(self, "BEGIN", closure).await
    }

    async fn run_atomic_with<F, R>(self, level: IsolationLevel, closure: F) -> Result<R, DjogiError>
    where
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send,
    {
        let begin_sql = begin_with_isolation_sql(level);
        run_pool_atomic_inner(self, &begin_sql, closure).await
    }
}

/// Common body for the pool-path `atomic` / `atomic_with` entry points.
/// `begin_sql` is the verbatim BEGIN statement — either `"BEGIN"` for
/// the default-isolation path or `BEGIN ISOLATION LEVEL <level>` for
/// the explicit-isolation path composed by
/// [`begin_with_isolation_sql`]. Centralising the body keeps the
/// commit / rollback / panic semantics in lockstep across both
/// entry points.
async fn run_pool_atomic_inner<F, R>(
    pool: &DjogiPool,
    begin_sql: &str,
    closure: F,
) -> Result<R, DjogiError>
where
    R: Send + 'static,
    F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send,
{
    // Acquire a connection and begin a transaction.
    let conn = pool.get().await?;
    let mut guard = TopLevelAtomicGuard::from_connection(conn, "pool");
    guard.conn_mut().batch_execute(begin_sql).await?;
    guard.promote_to_context(DjogiContext::from_connection);

    // Poll the closure through `catch_unwind` so a panic turns into
    // a caught payload. See the module-level panic-semantics docs.
    let result = AssertUnwindSafe(closure(guard.tx_ctx_mut()))
        .catch_unwind()
        .await;

    match result {
        Ok(Ok(value)) => {
            // Closure succeeded — commit and drain on-commit queue.
            guard.commit().await?;
            Ok(value)
        }
        Ok(Err(err)) => {
            // Closure returned Err — roll the transaction back and
            // surface the original error.
            if let Err(rb_err) = guard.rollback().await {
                tracing::error!(
                    error = ?rb_err,
                    "atomic: rollback after closure Err failed; returning closure err",
                );
            }
            Err(err)
        }
        Err(panic_payload) => {
            // Closure panicked — roll back before resuming the
            // panic so the transaction doesn't leak.
            if let Err(rb_err) = guard.rollback().await {
                tracing::error!(
                    error = ?rb_err,
                    "atomic: rollback after closure panic failed; resuming panic",
                );
            }
            resume_unwind(panic_payload);
        }
    }
}

async fn run_pool_context_atomic<F, R>(
    ctx: &mut DjogiContext,
    begin_sql: &str,
    closure: F,
) -> Result<R, DjogiError>
where
    R: Send + 'static,
    F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send,
{
    let pool = ctx.pool().cloned().ok_or_else(|| {
        DjogiError::Db(DbError::other(
            "atomic(&mut ctx, ...) expected a pool-backed context",
        ))
    })?;

    let conn = pool.get().await?;
    let mut guard = PoolContextAtomicGuard::new(ctx, conn);
    guard.conn_mut().batch_execute(begin_sql).await?;
    guard.promote_to_context();

    let result = AssertUnwindSafe(closure(guard.tx_ctx_mut()))
        .catch_unwind()
        .await;

    match result {
        Ok(Ok(value)) => match guard.commit().await {
            Ok(()) => {
                guard.propagate_success_to_parent();
                Ok(value)
            }
            Err(err @ DjogiError::TransactionPoisoned { .. }) => {
                guard.clear_parent_trackers();
                Err(err)
            }
            Err(err) => Err(err),
        },
        Ok(Err(err)) => {
            if let Err(rb_err) = guard.rollback().await {
                tracing::error!(
                    error = ?rb_err,
                    "atomic: rollback after closure Err failed; returning closure err",
                );
            }
            guard.clear_parent_trackers();
            Err(err)
        }
        Err(panic_payload) => {
            if let Err(rb_err) = guard.rollback().await {
                tracing::error!(
                    error = ?rb_err,
                    "atomic: rollback after closure panic failed; resuming panic",
                );
            }
            guard.clear_parent_trackers();
            resume_unwind(panic_payload);
        }
    }
}

fn clear_pool_context_transaction_trackers(ctx: &mut DjogiContext) {
    ctx.tenant_set = false;
    ctx.applied_tenant_id = None;
}

struct NestedAtomicCancellationGuard {
    ctx: *mut DjogiContext,
    callbacks_before: usize,
    auth_snapshot: AuthStateSnapshot,
    depth_incremented: bool,
    armed: bool,
}

// SAFETY: the guard is stored inside the same `atomic()` future that owns the
// exclusive `&mut DjogiContext` borrow. Moving that future between executor
// threads moves the raw pointer and the borrow together; the guard only
// dereferences during drop after later-declared futures have released `ctx`.
unsafe impl Send for NestedAtomicCancellationGuard {}

impl NestedAtomicCancellationGuard {
    fn armed(
        ctx: &mut DjogiContext,
        callbacks_before: usize,
        auth_snapshot: AuthStateSnapshot,
    ) -> Self {
        Self {
            ctx: ctx as *mut DjogiContext,
            callbacks_before,
            auth_snapshot,
            depth_incremented: true,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn restore_parent_state(&mut self) {
        let callbacks_before = self.callbacks_before;
        let auth_snapshot = self.auth_snapshot.clone();
        // SAFETY: the guard is declared before every future that borrows `ctx`
        // in `run_nested_savepoint_atomic`. Rust drops later-declared awaited
        // futures before this guard, so by the time Drop reaches here the
        // closure/savepoint future no longer holds its `&mut DjogiContext`.
        let ctx = unsafe { &mut *self.ctx };
        ctx.truncate_on_commit_queue(callbacks_before);
        ctx.restore_auth_state(auth_snapshot);
    }
}

impl Drop for NestedAtomicCancellationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        self.restore_parent_state();

        // SAFETY: see `restore_parent_state`. This is the same raw context
        // pointer, reached only after the awaited future borrowing `ctx` has
        // been dropped.
        let ctx = unsafe { &mut *self.ctx };
        if self.depth_incremented {
            ctx.decrement_savepoint_depth();
            self.depth_incremented = false;
        }
        ctx.poison_transaction(NESTED_ATOMIC_CANCELLED_POISON_REASON);
    }
}

// ---------------------------------------------------------------------------
// Nested-context impl — savepoints + callback promotion.
// ---------------------------------------------------------------------------

impl IntoAtomicScope for &mut DjogiContext {
    async fn run_atomic<F, R>(self, closure: F) -> Result<R, DjogiError>
    where
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send,
    {
        if matches!(self.inner_mut(), ContextInner::Pool(_)) {
            return run_pool_context_atomic(self, "BEGIN", closure).await;
        }

        run_nested_savepoint_atomic(self, closure).await
    }

    async fn run_atomic_with<F, R>(self, level: IsolationLevel, closure: F) -> Result<R, DjogiError>
    where
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send,
    {
        match self.inner_mut() {
            ContextInner::Pool(_) => {
                let begin_sql = begin_with_isolation_sql(level);
                run_pool_context_atomic(self, &begin_sql, closure).await
            }
            // Postgres pins the isolation level at the outer `BEGIN`
            // for the entire transaction. `SAVEPOINT` does not open a
            // sub-transaction with its own isolation knob — issuing
            // `SET TRANSACTION ISOLATION LEVEL` mid-transaction
            // (after the first non-control statement, which by the
            // time control reaches here has already executed) is
            // rejected by Postgres with SQLSTATE `25001`. Reject in
            // the framework before any SQL flies so the caller gets
            // a typed error rather than a deferred SQL-error surprise.
            ContextInner::Transaction(_) => {
                Err(DjogiError::IsolationLevelOnNestedScope { requested: level })
            }
        }
    }
}

async fn run_nested_savepoint_atomic<F, R>(
    ctx: &mut DjogiContext,
    closure: F,
) -> Result<R, DjogiError>
where
    R: Send + 'static,
    F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send,
{
    debug_assert!(
        matches!(ctx.inner_mut(), ContextInner::Transaction(_)),
        "run_nested_savepoint_atomic invoked on a non-transaction inner",
    );

    // Nested path shares the parent context directly — inner
    // writes land on the same transaction, inner on_commit
    // callbacks land on the parent's queue. Snapshot before
    // entering the closure so we can truncate on rollback.
    let callbacks_before = ctx.on_commit_queue_len();

    // Snapshot auth-related state (auth, applied_tenant_id, tenant_set,
    // tenant_scope_suppressed) so savepoint rollback restores the
    // in-memory trackers to match the post-rollback GUC state.
    // Without this, an inner scope that does set_auth(org_b) and
    // triggers set_tenant("org_b") would leave ctx.applied_tenant_id
    // = Some("org_b") after ROLLBACK TO SAVEPOINT reverted the GUC
    // to the outer value — the next tenant-keyed query in the outer
    // scope would then short-circuit (matching applied_tenant_id)
    // and silently run under the wrong tenant. phase-
    // boundary fixup (.
    let auth_snapshot = ctx.snapshot_auth_state();

    // Push savepoint. Depth is incremented BEFORE the SQL so
    // `sp_<depth>` numbering starts at 1 for the first nested
    // level. `sp_<n>` is ASCII + underscore — safe unquoted.
    ctx.increment_savepoint_depth();
    let depth = ctx.savepoint_depth();
    let savepoint_name = format!("sp_{depth}");

    let mut cancel_guard =
        NestedAtomicCancellationGuard::armed(ctx, callbacks_before, auth_snapshot.clone());

    let sp_sql = format!("SAVEPOINT {savepoint_name}");
    let push_result = {
        // Declared after `cancel_guard`, so cancellation while awaiting
        // SAVEPOINT drops this future before the guard mutates `ctx`.
        let push_future = match ctx.inner_mut() {
            ContextInner::Transaction(conn) => conn.batch_execute(&sp_sql),
            // Unreachable because of the debug_assert above.
            ContextInner::Pool(_) => unreachable!("debug_assert above rules this out"),
        };
        push_future.await
    };
    if let Err(e) = push_result {
        ctx.decrement_savepoint_depth();
        cancel_guard.disarm();
        return Err(e);
    }

    let inner_result = {
        // The inner future borrows `ctx`. Keep it in a scope declared after
        // `cancel_guard`: if the caller drops this outer future, Rust drops
        // `inner_future` first, then the guard restores/poisons the parent.
        let inner_future = AssertUnwindSafe(closure(ctx)).catch_unwind();
        inner_future.await
    };

    match inner_result {
        Ok(Ok(value)) => {
            // Success — RELEASE SAVEPOINT. Inner callbacks stay on
            // the parent queue (promoted). Inner auth-state mutations
            // also stay in effect: the caller's atomic scope is now
            // the parent and the inner's choices (e.g., set_auth) are
            // the continuing context.
            let release_sql = format!("RELEASE SAVEPOINT {savepoint_name}");
            let release_res = {
                let release_future = match ctx.inner_mut() {
                    ContextInner::Transaction(conn) => conn.batch_execute(&release_sql),
                    ContextInner::Pool(_) => unreachable!(),
                };
                release_future.await
            };
            ctx.decrement_savepoint_depth();
            cancel_guard.disarm();
            release_res?;
            Ok(value)
        }
        Ok(Err(err)) => {
            // Closure returned Err — ROLLBACK TO SAVEPOINT then
            // RELEASE. Discard inner callbacks by truncating the
            // parent queue back to its pre-closure length, and
            // restore auth state so it matches the reverted GUC.
            ctx.truncate_on_commit_queue(callbacks_before);
            ctx.restore_auth_state(auth_snapshot.clone());
            let rb_sql = format!("ROLLBACK TO SAVEPOINT {savepoint_name}");
            let rel_sql = format!("RELEASE SAVEPOINT {savepoint_name}");
            let rollback_error = {
                let rollback_future = ctx.run_rollback_to_release(&rb_sql, &rel_sql);
                rollback_future.await
            };
            if let Some(rb_err) = rollback_error {
                tracing::error!(
                    error = ?rb_err,
                    "atomic: ROLLBACK TO SAVEPOINT after closure Err failed; \
                     returning closure err",
                );
            }
            ctx.decrement_savepoint_depth();
            cancel_guard.disarm();
            Err(err)
        }
        Err(panic_payload) => {
            // Closure panicked — same rollback-then-resume as the
            // pool impl, scoped to the savepoint. Restore auth state
            // before resuming so the parent scope (if it catches
            // the unwind) sees consistent ctx state.
            ctx.truncate_on_commit_queue(callbacks_before);
            ctx.restore_auth_state(auth_snapshot);
            let rb_sql = format!("ROLLBACK TO SAVEPOINT {savepoint_name}");
            let rel_sql = format!("RELEASE SAVEPOINT {savepoint_name}");
            let rollback_error = {
                let rollback_future = ctx.run_rollback_to_release(&rb_sql, &rel_sql);
                rollback_future.await
            };
            if let Some(rb_err) = rollback_error {
                tracing::error!(
                    error = ?rb_err,
                    "atomic: ROLLBACK TO SAVEPOINT after closure panic failed; \
                     resuming panic",
                );
            }
            ctx.decrement_savepoint_depth();
            cancel_guard.disarm();
            resume_unwind(panic_payload);
        }
    }
}

// ---------------------------------------------------------------------------
// Public API.
// ---------------------------------------------------------------------------

/// Run `closure` inside an atomic transaction scope.
/// Two shapes:
/// - `atomic(&mut pool_ctx, |ctx| Box::pin(async move { ... }))`
///   preferred outermost form when the caller already has a pool-backed
///   context. Opens a transaction, shares `pool_ctx`'s Sassi registry,
///   commits on `Ok`, rolls back on `Err`, and drains on-commit callbacks
///   after the commit.
/// - `atomic(&pool, |ctx| Box::pin(async move { ... }))` — compatibility
///   shortcut when no parent context exists. Opens a fresh top-level
///   transaction context.
/// - `atomic(&mut tx_ctx, |ctx| Box::pin(async move { ... }))` — nested.
///   Emits `SAVEPOINT sp_<depth>` on entry; `RELEASE` on `Ok`, `ROLLBACK TO
/// SAVEPOINT` + `RELEASE` on `Err`. On-commit callbacks registered inside
///   a nested scope are promoted to the outer queue on success, discarded on
///   `Err`.
/// # Panic semantics
/// If the closure panics, `atomic()` rolls back (or rolls back to the
/// savepoint, in the nested case) **before** the panic resumes. The
/// transaction never leaks. See the module-level docs for rationale.
/// # Examples
/// ```ignore
/// let mut ctx = DjogiContext::from_pool(pool.clone());
///
/// djogi::transaction::atomic(&mut ctx, |ctx| Box::pin(async move {
/// Account::create(ctx, Account { balance: 100, ..Default::default() }).await?;
/// Ok::<_, DjogiError>(())
/// }))
/// .await?;
/// ```
pub async fn atomic<S, F, R>(scope: S, closure: F) -> Result<R, DjogiError>
where
    S: IntoAtomicScope,
    R: Send + 'static,
    F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send,
{
    scope.run_atomic(closure).await
}

/// Run `closure` inside an atomic transaction scope, opening the
/// outermost transaction at the requested Postgres isolation level.
/// `atomic_with` is the sibling of [`atomic`] for callers that need an
/// explicit isolation level instead of Postgres' session default
/// (`READ COMMITTED`). The level is threaded into the outer `BEGIN`
/// statement as `BEGIN ISOLATION LEVEL <level>`; everything else
/// (commit/rollback semantics, panic safety, on-commit callback drain)
/// matches [`atomic`] exactly.
/// # Scopes
/// - `atomic_with(level, &mut pool_ctx, ...)` — preferred outermost
///   shape when the caller already holds a pool-backed
///   [`DjogiContext`]. Opens a transaction at `level`, shares the
///   parent context's Sassi registry, commits on `Ok`, rolls back on
///   `Err`.
/// - `atomic_with(level, &pool, ...)` — compatibility shortcut when
///   no parent context exists. Opens a fresh top-level transaction
///   context at `level`.
/// - `atomic_with(level, &mut tx_ctx, ...)` — **rejected** with
///   [`DjogiError::IsolationLevelOnNestedScope`]. Postgres pins
///   isolation at the outer `BEGIN` for the entire transaction;
///   `SAVEPOINT` does not open a sub-transaction with its own
///   isolation knob. Use [`atomic`] for nested scopes — the nested
///   scope inherits the outermost transaction's isolation level.
/// # Retry composition
/// Both `RepeatableRead` and `Serializable` can raise SQLSTATE
/// `40001` on commit-time conflict. Wrap the `atomic_with` call in
/// [`retry_on_conflict`] to re-run on `40001`:
/// ```ignore
/// use djogi::transaction::{atomic_with, retry_on_conflict, IsolationLevel};
///
/// retry_on_conflict(&mut ctx, 3, async |ctx| {
/// atomic_with(IsolationLevel::Serializable, ctx, |tx| Box::pin(async move {
/// // ... reads + writes that must observe a serial schedule ...
/// Ok::<_, DjogiError>(())
/// })).await
/// }).await?;
/// ```
/// # Panic semantics
/// Identical to [`atomic`]: a closure panic triggers `ROLLBACK`
/// before the panic resumes; the transaction never leaks.
/// # Errors
/// - [`DjogiError::IsolationLevelOnNestedScope`] when invoked on a
///   transaction-backed `DjogiContext` (the nested savepoint scope).
///   Classified as **terminal** — retrying cannot turn a nested
///   savepoint into a fresh outermost transaction.
/// - The underlying `BEGIN ISOLATION LEVEL <level>` SQL error if
///   Postgres refuses the request (`25001` after the first
///   statement, etc.). Classified by Postgres' SQLSTATE.
/// # Example
/// ```ignore
/// use djogi::DjogiContext;
/// use djogi::transaction::{atomic_with, IsolationLevel};
///
/// let mut ctx = DjogiContext::from_pool(pool.clone());
///
/// atomic_with(IsolationLevel::Serializable, &mut ctx, |ctx| Box::pin(async move {
/// // SSI snapshot is fixed at the first statement. Concurrent
/// // writes that violate a multi-row invariant raise 40001 at
/// // commit time; wrap in retry_on_conflict to retry.
/// let total = Account::objects().sum(ctx, |f| f.balance()).await?;
/// // ... act on `total` ...
/// Ok::<_, DjogiError>(())
/// })).await?;
/// ```
pub async fn atomic_with<S, F, R>(
    level: IsolationLevel,
    scope: S,
    closure: F,
) -> Result<R, DjogiError>
where
    S: IntoAtomicScope,
    R: Send + 'static,
    F: for<'a> FnOnce(&'a mut DjogiContext) -> AtomicFuture<'a, R> + Send,
{
    scope.run_atomic_with(level, closure).await
}

/// Re-run `closure` up to `attempts` times on lock / serialization
/// conflicts.
/// Classifies errors via [`crate::DjogiError::is_transient`] — SQLSTATEs
/// `40001`, `40P01`, `55P03` are considered retryable. Every other
/// error (constraint violations, not-found, etc.) surfaces on the first
/// call. This helper retries immediately; use
/// [`retry_on_conflict_with_backoff`] when production contention or
/// [`crate::DjogiError::PoolTimeout`] should sleep before retrying.
pub async fn retry_on_conflict<F, R>(
    ctx: &mut DjogiContext,
    attempts: u32,
    mut closure: F,
) -> Result<R, DjogiError>
where
    F: AsyncFnMut(&mut DjogiContext) -> Result<R, DjogiError>,
{
    debug_assert!(attempts >= 1, "attempts must be >= 1");
    let mut attempt: u32 = 0;
    loop {
        attempt = attempt.saturating_add(1);
        match closure(ctx).await {
            Ok(value) => return Ok(value),
            Err(e) => {
                let retryable = e.is_transient();
                if retryable && attempt < attempts {
                    tracing::debug!(
                        attempt,
                        attempts,
                        "retry_on_conflict: retryable lock error; retrying",
                    );
                    continue;
                }
                return Err(e);
            }
        }
    }
}

/// Re-run `closure` on transient transaction errors, sleeping between retries.
/// This is the production-oriented sibling of [`retry_on_conflict`]. The
/// original helper intentionally performs immediate retries; this helper uses a
/// configurable [`TransactionRetryBackoff`] so `PoolTimeout` retries do not
/// tight-loop against a saturated pool. The policy also carries the retryable
/// error class set, so callers can explicitly include or exclude classes such
/// as `PoolTimeout`.
/// ```ignore
/// use djogi::transaction::{
/// atomic_with, retry_on_conflict_with_backoff, IsolationLevel,
/// TransactionRetryBackoff,
/// };
///
/// retry_on_conflict_with_backoff(
/// &mut ctx,
/// 5,
/// TransactionRetryBackoff::default(),
/// async |ctx| {
/// atomic_with(IsolationLevel::Serializable, ctx, |tx| Box::pin(async move {
/// // ... reads + writes ...
/// Ok::<_, DjogiError>(())
/// })).await
/// },
/// ).await?;
/// ```
pub async fn retry_on_conflict_with_backoff<F, R>(
    ctx: &mut DjogiContext,
    attempts: u32,
    policy: TransactionRetryBackoff,
    mut closure: F,
) -> Result<R, DjogiError>
where
    F: AsyncFnMut(&mut DjogiContext) -> Result<R, DjogiError>,
{
    debug_assert!(attempts >= 1, "attempts must be >= 1");
    let mut attempt: u32 = 0;
    loop {
        attempt = attempt.saturating_add(1);
        match closure(ctx).await {
            Ok(value) => return Ok(value),
            Err(e) => {
                let retryable = policy.should_retry(&e);
                if retryable && attempt < attempts {
                    let delay = policy
                        .delay_for_retry(&e, attempt)
                        .unwrap_or(Duration::ZERO);
                    tracing::debug!(
                        attempt,
                        attempts,
                        delay_ms = delay.as_millis(),
                        error_kind = retry_error_kind(&e),
                        "retry_on_conflict_with_backoff: retryable transaction error; sleeping before retry",
                    );
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    continue;
                }
                return Err(e);
            }
        }
    }
}

fn retry_error_kind(error: &DjogiError) -> &'static str {
    match error {
        DjogiError::PoolTimeout { .. } => "pool_timeout",
        DjogiError::LockConflict(_) => "lock_conflict",
        DjogiError::Db(_) => "database_transient",
        _ => "transient",
    }
}

// ---------------------------------------------------------------------------
// Internal helpers bolted onto `DjogiContext` for the nested path.
// ---------------------------------------------------------------------------

impl DjogiContext {
    /// Current length of the on-commit callback queue.
    pub(crate) fn on_commit_queue_len(&self) -> usize {
        self.on_commit_len()
    }

    /// Truncate the on-commit callback queue back to `new_len`.
    pub(crate) fn truncate_on_commit_queue(&mut self, new_len: usize) {
        self.on_commit_truncate(new_len);
    }

    /// Issue `ROLLBACK TO SAVEPOINT` followed by `RELEASE SAVEPOINT`.
    /// Returns `Some(err)` on the first failure, `None` if both
    /// succeeded.
    async fn run_rollback_to_release(&mut self, rb_sql: &str, rel_sql: &str) -> Option<DjogiError> {
        match self.inner_mut() {
            ContextInner::Transaction(conn) => {
                if let Err(e) = conn.batch_execute(rb_sql).await {
                    return Some(e);
                }
                if let Err(e) = conn.batch_execute(rel_sql).await {
                    return Some(e);
                }
                None
            }
            ContextInner::Pool(_) => Some(DjogiError::Db(DbError::other(
                "run_rollback_to_release called on a pool-backed context; \
                 this is a framework-invariant bug",
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    // ── IsolationLevel — SQL keyword + composition tests ─────────────────
    // Pins the SQL grammar at the framework boundary: a regression in
    // `as_sql_keyword` would emit malformed SQL ("BEGIN ISOLATION
    // LEVEL Serializable" is rejected — Postgres needs the SQL
    // standard's two-word forms). These are no-DB tests so they run
    // in the unit-test pass without requiring a live Postgres.

    #[test]
    fn isolation_level_keywords_match_postgres_grammar() {
        assert_eq!(
            IsolationLevel::ReadCommitted.as_sql_keyword(),
            "READ COMMITTED"
        );
        assert_eq!(
            IsolationLevel::RepeatableRead.as_sql_keyword(),
            "REPEATABLE READ"
        );
        assert_eq!(
            IsolationLevel::Serializable.as_sql_keyword(),
            "SERIALIZABLE"
        );
    }

    #[test]
    fn isolation_level_display_matches_keyword() {
        assert_eq!(IsolationLevel::ReadCommitted.to_string(), "READ COMMITTED");
        assert_eq!(
            IsolationLevel::RepeatableRead.to_string(),
            "REPEATABLE READ"
        );
        assert_eq!(IsolationLevel::Serializable.to_string(), "SERIALIZABLE");
    }

    #[test]
    fn begin_with_isolation_sql_composes_with_keyword() {
        assert_eq!(
            begin_with_isolation_sql(IsolationLevel::ReadCommitted),
            "BEGIN ISOLATION LEVEL READ COMMITTED",
        );
        assert_eq!(
            begin_with_isolation_sql(IsolationLevel::RepeatableRead),
            "BEGIN ISOLATION LEVEL REPEATABLE READ",
        );
        assert_eq!(
            begin_with_isolation_sql(IsolationLevel::Serializable),
            "BEGIN ISOLATION LEVEL SERIALIZABLE",
        );
    }

    #[test]
    fn default_retry_backoff_treats_pool_timeout_as_stronger_pressure_signal() {
        let policy = TransactionRetryBackoff::default().with_jitter(Duration::ZERO);
        let lock_error = DjogiError::LockConflict(DbError::other("synthetic lock conflict"));
        let pool_error = DjogiError::PoolTimeout { phase: "wait" };

        let lock_delay = policy.base_delay_for_retry(&lock_error, 1).unwrap();
        let pool_delay = policy.base_delay_for_retry(&pool_error, 1).unwrap();

        assert!(
            pool_delay > lock_delay,
            "PoolTimeout retry delay must exceed lock-conflict delay by default"
        );
    }

    #[test]
    fn retry_backoff_is_exponential_and_capped() {
        let policy = TransactionRetryBackoff::none()
            .with_lock_conflict_initial_delay(Duration::from_millis(10))
            .with_max_delay(Duration::from_millis(25));
        let error = DjogiError::LockConflict(DbError::other("synthetic lock conflict"));

        assert_eq!(
            policy.base_delay_for_retry(&error, 1),
            Some(Duration::from_millis(10)),
        );
        assert_eq!(
            policy.base_delay_for_retry(&error, 2),
            Some(Duration::from_millis(20)),
        );
        assert_eq!(
            policy.base_delay_for_retry(&error, 3),
            Some(Duration::from_millis(25)),
        );
    }

    #[test]
    fn retry_backoff_ignores_terminal_errors() {
        let policy = TransactionRetryBackoff::default();
        let error = DjogiError::Validation("not retryable".to_string());

        assert_eq!(policy.base_delay_for_retry(&error, 1), None);
    }

    #[test]
    fn retry_backoff_retry_classes_can_disable_pool_timeout() {
        let policy = TransactionRetryBackoff::default().with_retryable_error_classes(
            RetryableErrorClasses::default().with_pool_timeout(false),
        );
        let lock_error = DjogiError::LockConflict(DbError::other("synthetic lock conflict"));
        let pool_error = DjogiError::PoolTimeout { phase: "wait" };

        assert!(
            policy.base_delay_for_retry(&lock_error, 1).is_some(),
            "lock conflicts remain retryable with the default class set",
        );
        assert_eq!(
            policy.base_delay_for_retry(&pool_error, 1),
            None,
            "pool timeout retries must be policy-controlled and can be disabled",
        );
    }

    #[test]
    fn retry_backoff_jitter_can_span_multiple_seconds() {
        let max_nanos = Duration::from_secs(2).as_nanos();
        let mut saw_above_one_second = false;

        for seed in 0_u128..128 {
            let jitter = mixed_jitter_nanos(seed, max_nanos);
            assert!(
                jitter <= max_nanos,
                "jitter must not exceed configured max, got {jitter} > {max_nanos}",
            );
            saw_above_one_second |= jitter > Duration::from_secs(1).as_nanos();
        }

        assert!(
            saw_above_one_second,
            "jitter mixing must not be capped to the subsecond range",
        );
    }

    async fn retry_helper_test_context() -> DjogiContext {
        let pool = DjogiPool::builder("postgres://localhost/_djogi_unreachable")
            .max_size(1)
            .build()
            .await
            .expect("pool construction should not connect until checkout");
        DjogiContext::from_pool(pool)
    }

    #[tokio::test]
    async fn retry_on_conflict_with_backoff_returns_first_try_success_without_retry() {
        let mut ctx = retry_helper_test_context().await;
        let calls = Arc::new(AtomicU32::new(0));
        let observed_calls = calls.clone();

        let value = retry_on_conflict_with_backoff(
            &mut ctx,
            3,
            TransactionRetryBackoff::none(),
            async move |_| {
                calls.fetch_add(1, AtomicOrdering::SeqCst);
                Ok::<_, DjogiError>(42)
            },
        )
        .await
        .expect("first attempt should succeed");

        assert_eq!(value, 42);
        assert_eq!(observed_calls.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retry_on_conflict_with_backoff_short_circuits_terminal_error() {
        let mut ctx = retry_helper_test_context().await;
        let calls = Arc::new(AtomicU32::new(0));
        let observed_calls = calls.clone();

        let err = retry_on_conflict_with_backoff(
            &mut ctx,
            5,
            TransactionRetryBackoff::none(),
            async move |_| {
                calls.fetch_add(1, AtomicOrdering::SeqCst);
                Err::<(), _>(DjogiError::Validation("terminal".to_string()))
            },
        )
        .await
        .expect_err("terminal error must surface immediately");

        assert!(matches!(err, DjogiError::Validation(ref msg) if msg == "terminal"));
        assert_eq!(observed_calls.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retry_on_conflict_with_backoff_recovers_after_retryable_error() {
        let mut ctx = retry_helper_test_context().await;
        let calls = Arc::new(AtomicU32::new(0));
        let observed_calls = calls.clone();

        let value = retry_on_conflict_with_backoff(
            &mut ctx,
            3,
            TransactionRetryBackoff::none(),
            async move |_| {
                let completed = calls.fetch_add(1, AtomicOrdering::SeqCst);
                if completed == 0 {
                    Err(DjogiError::PoolTimeout { phase: "wait" })
                } else {
                    Ok(completed + 1)
                }
            },
        )
        .await
        .expect("second attempt should recover");

        assert_eq!(value, 2);
        assert_eq!(observed_calls.load(AtomicOrdering::SeqCst), 2);
    }

    #[tokio::test]
    async fn retry_on_conflict_with_backoff_policy_can_disable_pool_timeout_retries() {
        let mut ctx = retry_helper_test_context().await;
        let calls = Arc::new(AtomicU32::new(0));
        let observed_calls = calls.clone();
        let policy = TransactionRetryBackoff::none().with_retryable_error_classes(
            RetryableErrorClasses::default().with_pool_timeout(false),
        );

        let err = retry_on_conflict_with_backoff(&mut ctx, 5, policy, async move |_| {
            calls.fetch_add(1, AtomicOrdering::SeqCst);
            Err::<(), _>(DjogiError::PoolTimeout { phase: "wait" })
        })
        .await
        .expect_err("PoolTimeout should surface immediately when disabled");

        assert!(matches!(err, DjogiError::PoolTimeout { phase: "wait" }));
        assert_eq!(
            observed_calls.load(AtomicOrdering::SeqCst),
            1,
            "retry class policy must prevent incidental pool-timeout retries",
        );
    }

    // ── ConstraintMode — SQL keyword tests ──────────────────────────────

    #[test]
    fn constraint_mode_keywords_match_postgres_grammar() {
        assert_eq!(ConstraintMode::Deferred.as_sql_keyword(), "DEFERRED");
        assert_eq!(ConstraintMode::Immediate.as_sql_keyword(), "IMMEDIATE");
    }

    // ── DeferScope — equality + variant pin tests ───────────────────────

    #[test]
    fn defer_scope_all_is_value_equal() {
        // `DeferScope` is `Copy + Eq`. Two `All` values must compare
        // equal regardless of how the call site constructed them.
        assert_eq!(DeferScope::All, DeferScope::All);
    }

    #[test]
    fn defer_scope_named_compares_by_slice_contents() {
        // Equality on `Named` uses pointer + length equality of the
        // slice — two slices with identical contents but distinct
        // static lifetimes would be equal; the compiler dedupes
        // identical literal slices.
        static A: &[&str] = &["posts_author_id_fkey"];
        static B: &[&str] = &["posts_author_id_fkey"];
        assert_eq!(DeferScope::Named(A), DeferScope::Named(B));
    }
}
