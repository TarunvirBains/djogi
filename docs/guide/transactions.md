> [Back to Guides](./index.md) · [Back to README](../../ReadMe.MD)

# Transactions

Phase 4's transaction substrate: `DjogiContext`, `atomic()`, savepoint
nesting, `on_commit` callbacks, row locks, and `retry_on_conflict`.

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

## Error classification

`DjogiError::is_transient()` / `is_terminal()` classify whether a
retry of the same closure may succeed. `LockConflict` and raw
`Db(DbError)` with SQLSTATE `40001` / `40P01` / `55P03` are transient;
everything else is terminal. `retry_on_conflict(ctx, attempts,
closure)` drives retry using the same predicate.

```rust
djogi::transaction::retry_on_conflict(ctx, 3, |ctx| async move {
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
