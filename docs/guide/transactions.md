> [Back to Guides](./index.md) · [Back to README](../../ReadMe.MD)

# Transactions

Phase 4's transaction substrate plus Phase 8.5's transaction-control
helpers: `DjogiContext`, `atomic()` / `atomic_with()`, savepoint
nesting, on-commit callbacks, row locks, isolation levels, deferred
constraints, concurrent-reads cloning, `retry_on_conflict`, and
`retry_on_conflict_with_backoff`.

## Contract

Every CRUD and `QuerySet` terminal takes `&mut DjogiContext`. A
`DjogiContext` holds either a pool handle (auto-commit per statement)
or an active `tokio_postgres::Transaction` (all writes commit together
on `atomic()`'s success return, or roll back on `Err` / panic).

- `atomic(executor, closure)` — enters a transactional scope. Passes
  `&mut DjogiContext` to the closure. Prefer `atomic(&mut ctx, ...)`
  when you already have a request context; it preserves that context's
  Sassi/Punnu registry. `atomic(&pool, ...)` remains available as a
  fresh top-level context shortcut.
- `ctx.on_commit(|| ...)` — queue a callback to fire after the
  outermost transaction commits. Inner savepoints do NOT flush
  callbacks; only the root atomic scope does.
- Nested `atomic(&mut ctx, ...)` — uses Postgres savepoints
  (`SAVEPOINT sp_N`). Panics or `Err` return rolls back the savepoint
  only; the outer scope continues.

## Golden path

```rust
use djogi::prelude::*;

let mut ctx = DjogiContext::from_pool(pool.clone());

djogi::transaction::atomic(&mut ctx, |ctx| Box::pin(async move {
    let post = Post::create(ctx, Post { title: "hi".into(), ..Default::default() }).await?;
    post.save(ctx).await?;

    ctx.on_commit(move || {
        println!("post {} committed", post.id);
    });

    Ok::<_, DjogiError>(())
})).await?;
```

Commits on `Ok(_)`; rolls back on `Err(_)` or panic. `on_commit`
callbacks fire *after* commit returns — they cannot fail the
transaction.

## Row locks

`QuerySet<T>` has three `#[must_use]` lock builders. Last call wins.

```rust
let row = Post::objects()
    .filter(|f| f.id().eq(post_id))
    .select_for_update()     // FOR UPDATE
    .fetch_one(ctx).await?;

let row = Post::objects()
    .nowait()                // FOR UPDATE NOWAIT — fail fast on lock
    .fetch_one(ctx).await?;

let jobs = Queue::objects()
    .filter(|f| f.status().eq("ready"))
    .skip_locked()           // FOR UPDATE SKIP LOCKED — work-queue shape
    .fetch_all(ctx).await?;
```

Row locks are only meaningful inside `atomic()`. On a pool-backed
context, the lock releases immediately when the implicit per-statement
transaction closes — no protection against concurrent writers.

## Isolation levels — `atomic_with`

`atomic()` opens the outermost transaction at Postgres' session
default isolation level (typically `READ COMMITTED`). When a scope
needs `REPEATABLE READ` or `SERIALIZABLE`, use `atomic_with`:

```rust
use djogi::prelude::*;
use djogi::transaction::{atomic_with, IsolationLevel};

atomic_with(IsolationLevel::Serializable, &mut ctx, |ctx| Box::pin(async move {
    // Serializable Snapshot Isolation — Postgres aborts a conflicting
    // transaction with SQLSTATE 40001 if the interleaving could not be
    // reproduced by some serial schedule.
    let total = Account::objects().sum(ctx, |f| f.balance()).await?;
    Account::create(ctx, Account { balance: total / 2, ..Default::default() }).await?;
    Ok::<_, DjogiError>(())
})).await?;
```

### Variant matrix

| `IsolationLevel`    | Postgres keyword    | Snapshot fixed at | Serialization-failure surface     |
|---------------------|---------------------|-------------------|------------------------------------|
| `ReadCommitted`     | `READ COMMITTED`    | Each statement    | Never (statement-local snapshot)   |
| `RepeatableRead`    | `REPEATABLE READ`   | First statement   | `40001` at conflict — commit time  |
| `Serializable`      | `SERIALIZABLE`      | First statement   | `40001` via SSI — commit or read   |

`READ UNCOMMITTED` is intentionally not exposed: Postgres aliases it
to `READ COMMITTED` server-side, so it offers no weaker guarantees.

### Retry composition

Both `RepeatableRead` and `Serializable` raise SQLSTATE `40001` on
commit-time conflict. Wrap `atomic_with` in `retry_on_conflict` so the
typed isolation surface participates in the standard retry loop:

```rust
use djogi::transaction::{atomic_with, retry_on_conflict, IsolationLevel};

retry_on_conflict(&mut ctx, 3, async |ctx| {
    atomic_with(IsolationLevel::Serializable, ctx, |tx| Box::pin(async move {
        // ... reads + writes that must observe a serial schedule ...
        Ok::<_, DjogiError>(())
    })).await
}).await?;
```

`is_transient()` classifies `40001` as retryable; the loop re-runs
the closure up to `attempts` times.

`retry_on_conflict` deliberately retries immediately. That is useful
for small lock-conflict retry loops, but it is the wrong default when
the transient error is `DjogiError::PoolTimeout`: immediately
re-entering a saturated pool can add pressure faster than connections
free up.

Use `retry_on_conflict_with_backoff` for production paths where the
closure may need to check out a connection from a busy pool:

```rust
use djogi::transaction::{
    atomic_with, retry_on_conflict_with_backoff, IsolationLevel,
    TransactionRetryBackoff,
};

retry_on_conflict_with_backoff(
    &mut ctx,
    5,
    TransactionRetryBackoff::default(),
    async |ctx| {
        atomic_with(IsolationLevel::Serializable, ctx, |tx| Box::pin(async move {
            // ... reads + writes that may hit lock conflict or PoolTimeout ...
            Ok::<_, DjogiError>(())
        })).await
    },
).await?;
```

The default backoff is dependency-free and treats pool saturation as a
stronger pressure signal than row-lock conflict: `PoolTimeout` starts
with a longer delay, both classes grow exponentially up to a cap, and
small additive jitter reduces synchronized retry bursts. Tune with
`TransactionRetryBackoff::default()
    .with_pool_timeout_initial_delay(...)
    .with_lock_conflict_initial_delay(...)
    .with_max_delay(...)
    .with_jitter(...)`.

### SAVEPOINT vs SET LOCAL semantics

`atomic_with(level, &mut tx_ctx, ...)` — **rejected** with
[`DjogiError::IsolationLevelOnNestedScope`]. Postgres pins the
isolation level for the entire transaction at the outer `BEGIN`;
`SAVEPOINT` does not open a sub-transaction with its own isolation
knob. Use `atomic()` for nested scopes — the savepoint inherits the
outermost transaction's isolation level.

## Deferred constraints — `defer_constraints`

`SET CONSTRAINTS` is a Postgres transaction-control statement that
flips deferrable constraints between IMMEDIATE (check at every
statement) and DEFERRED (check at COMMIT). The canonical use case is
the **circular FK** pattern: two rows with reciprocal FKs that cannot
be inserted in either order without temporarily violating referential
integrity.

```rust
use djogi::prelude::*;
use djogi::transaction::{atomic, DeferScope};

atomic(&mut ctx, |ctx| Box::pin(async move {
    // Defer ALL deferrable constraints to commit time for the
    // remainder of this transaction.
    ctx.defer_constraints(DeferScope::All).await?;

    // Insert the cycle. Neither row names a parent that exists yet,
    // but Postgres only checks the FKs at commit — by then both
    // rows are present and the cycle resolves.
    let a = NodeA::create(ctx, NodeA { peer_b: peer_b_id, ..Default::default() }).await?;
    let b = NodeB::create(ctx, NodeB { peer_a: a.id, ..Default::default() }).await?;

    Ok::<_, DjogiError>(())
})).await?;
```

### `DeferScope::Named` and typed validation

For finer-grained control, target specific constraints by name. The
framework validates each name against the model-descriptor inventory
before any SQL is emitted:

```rust
ctx.defer_constraints(
    DeferScope::Named(&["posts_author_id_fkey", "comments_post_id_fkey"]),
).await?;
```

The validator checks:

- **Unknown name** → [`DjogiError::UnknownConstraintName`]. The
  expected shape is `<table>_<column>_fkey` (Postgres' convention)
  for names that fit inside Postgres' 63-byte identifier limit. When
  the conventional `<table>_<column>_fkey` would exceed 63 bytes, the
  framework substitutes a deterministic 54-byte prefix plus an 8-char
  hex digest — Postgres' own tail-truncation rule is **not** used,
  because two distinct constraints can otherwise collide post-
  truncation. The migration emitter writes the same name explicitly
  (`CONSTRAINT <name> REFERENCES ...`), so the runtime validator and
  the on-disk DDL stay byte-for-byte in lockstep at any name length.
- **Non-deferrable constraint** →
  [`DjogiError::ConstraintNotDeferrable`]. Declare the FK as
  `#[field(deferrable = true)]` (and optionally
  `initially_deferred = true`) at the model declaration.

This is the typed-surface value-add over
`raw_execute("SET CONSTRAINTS ...")` — Postgres would raise `42704`
or `0A000` after a round trip; the framework surfaces the misuse
synchronously with the model-declaration remediation hint.

### Mirror: `set_constraints_immediate`

`set_constraints_immediate(scope)` reverses an earlier
`defer_constraints` call. Same scope semantics; useful for forcing
constraint checks at a specific point mid-transaction rather than
waiting for COMMIT.

### Transaction-scope-only invariant

Both helpers reject pool-backed contexts with
[`DjogiError::ConstraintModeOutsideTransaction`]. `SET CONSTRAINTS`
is transaction-scoped; outside a transaction it would either
evaporate after the implicit statement-transaction or fail outright.
Wrap the call in `atomic()` so the helper has a transaction to bind
to.

## Concurrent reads — `clone_for_concurrent_reads`

`&mut DjogiContext` is exclusive, so the natural `tokio::try_join!`
shape over two typed reads on the same context fails to compile
(`E0499`):

```rust
// Does NOT compile — both branches need `&mut ctx`.
let (alpha, beta) = tokio::try_join!(
    Post::objects().filter(|f| f.kind().eq("alpha")).fetch_all(&mut ctx),
    Post::objects().filter(|f| f.kind().eq("beta")).fetch_all(&mut ctx),
)?;
```

Clone the context first so each branch has its own pool-backed
handle:

```rust
let mut ctx_a = ctx.clone_for_concurrent_reads()?;
let mut ctx_b = ctx.clone_for_concurrent_reads()?;
let (alpha, beta) = tokio::try_join!(
    Post::objects().filter(|f| f.kind().eq("alpha")).fetch_all(&mut ctx_a),
    Post::objects().filter(|f| f.kind().eq("beta")).fetch_all(&mut ctx_b),
)?;
```

Each clone draws from the same pool but checks out its own
connection per operation — the two futures run truly concurrently
without aliasing one transaction's connection.

### What carries over

- **Pool** — same `DjogiPool` (Arc-cloned). Independent checkouts.
- **`Sassi` cache registry** — same `Arc<Sassi>`. Cache writes
  through one clone are visible to reads through the other.
- **Auth context** — cloned. RLS still applies.
- **Tenant-scope-suppression flag** — copied.

### What does NOT carry over

- **Transaction-scoped state** (`applied_tenant_id`, `tenant_set`) —
  resets to none on the clone. The clone is pool-backed; it has no
  transaction.
- **`on_commit` queue** — each clone owns its own queue.
- **Savepoint depth** — clones start at zero.

### Transaction-context rejection

`clone_for_concurrent_reads` is only valid on pool-backed contexts —
calling it on a transaction-backed context returns
[`DjogiError::ConcurrentReadsRequirePoolContext`]. A transaction
owns one Postgres connection; cloning would either alias that
connection across futures (protocol violation) or silently break the
transaction boundary. Move the concurrent-reads block outside the
surrounding `atomic()`, or fetch sequentially within the transaction.

## Error classification

`DjogiError::is_transient()` / `is_terminal()` classify whether a
retry of the same closure may succeed. `LockConflict` and raw
`Db(DbError)` with SQLSTATE `40001` / `40P01` / `55P03` are transient;
everything else (including all the Phase 8.5 transaction-control
typed errors above) is terminal. `retry_on_conflict(ctx, attempts,
closure)` drives retry using the same predicate.

```rust
djogi::transaction::retry_on_conflict(ctx, 3, async |ctx| {
    let row = Post::objects()
        .filter(|f| f.id().eq(id))
        .nowait()
        .fetch_one(ctx).await?;     // LockConflict on 55P03
    row.save(ctx).await?;
    Ok::<_, DjogiError>(())
}).await?;
```

No backoff in Phase 4 — pure retry. Exponential / jittered backoff is
deferred until measured necessity.

## Convenience

Every `T: Model` gets (emitted per-model by the macro):

- `T::bulk_create(ctx, rows)` / `T::bulk_update(ctx, ids, closure)` /
  `T::bulk_upsert(ctx, rows, &[conflict_col])` — multi-row write paths
  with `RETURNING *` rehydration where applicable.
- `T::create_or_find(ctx, row)` + `T::bulk_upsert_by_descriptor(ctx,
  rows)` — for models with `#[model(idempotency_key = "col")]`, keys
  the conflict target off the descriptor slot.
- `QuerySet<T>::get_or_create(ctx, factory)` / `update_or_create(ctx,
  factory, updater)` / `in_bulk(ctx, ids)` — filter-chained
  convenience terminals.

See [expressions](./expressions.md) for expression-backed bulk UPDATE
assignments.
