> [Back to README](../../ReadMe.MD) | [All Guides](./index.md)

# Connection Pool — DjogiPool

`DjogiPool` is the framework's Postgres connection pool. It wraps
`deadpool_postgres::Pool` with a Djogi-specific builder, a
`post_connect` hook for per-physical-connection setup, a
`with_client` raw-borrow escape hatch for operations that cannot
route through `DjogiContext`, and a config-driven entry point that
walks `env > Djogi.toml > builder default` for sizing.

This guide covers the public surface introduced in Phase 8-Zero.
For the broader context — `DjogiContext`, transactions, raw queries —
see the [Transactions guide](./transactions.md) and the
[Getting Started guide](./getting-started.md).

---

## What you get

```rust
use djogi::pg::pool::DjogiPool;
use std::time::Duration;

let pool = DjogiPool::builder("postgres://localhost/myapp")
    .max_size(20)
    .timeout(Duration::from_secs(5))
    .post_connect(|client| Box::pin(async move {
        client.batch_execute("SET statement_timeout = '5s'").await?;
        Ok(())
    }))
    .build()
    .await?;
```

Defaults if you skip the builder knobs (or call `DjogiPool::connect(url)`):

- `max_size = 5` (`DjogiPool::DEFAULT_MAX_SIZE`)
- no wait timeout (callers block until a slot is available)
- no `post_connect` hook

The defaults are dev-friendly. Production deployments size against
their concurrent-request budget and bound the wait so a saturated
pool fails fast.

---

## Sizing — `max_size`

`max_size` is the hard cap on physical connections to Postgres.

### How to pick a number

Match the pool to your service's concurrent database-touching tasks,
NOT to your CPU count. A web server handling 200 concurrent requests
that each issue 2-3 sequential queries needs roughly 30-50
connections, not 8. The math:

```
max_size ≈ p99 concurrent requests × queries-per-request × p99 query time
```

If your DB box has a `max_connections` cap of its own (Postgres
default is 100), keep the sum of every service's `max_size` below
that, leaving headroom for psql, monitoring, and replication.

### Zero is rejected

`max_size(0)` returns `DjogiError::Validation` from `.build()` —
deadpool's internal semaphore would have zero permits and every
checkout would hang forever. The error fires at construction time
rather than as a mysterious hang at first query.

---

## Bounding the wait — `timeout`

`.timeout(Duration)` sets deadpool's wait timeout. When the pool is
saturated and no slot frees within the deadline, the originating
checkout returns `DjogiError::PoolTimeout { phase: "wait" }`.

Production budget: pick something in the 1-10 second range. Long
enough to absorb burst load, short enough that a tipping-over service
sheds queue depth instead of accumulating it. `DjogiError::PoolTimeout`
is classified as **transient** by `is_transient()` — generic retry
helpers see it as a back-off-and-retry condition, not a permanent
failure.

The `phase` field carries `"wait"`, `"create"`, or `"recycle"`
identifying which deadpool timeout fired:

- `"wait"` — pool at `max_size`, no slot freed in time. Tune
  `max_size` upward or stop holding connections across awaits
  unrelated to the database.
- `"create"` — `Manager::create` (opening a fresh socket) timed out.
  Network or DB-side problem, not pool sizing.
- `"recycle"` — recycling on the checkout path timed out. Same root
  cause as `"create"` for `Verified`/`Clean` recycling methods.

---

## Per-connection setup — `post_connect`

`.post_connect(closure)` runs once per physical connection. It does
NOT fire on the per-checkout path — exactly the right semantic for
one-time `SET` statements:

```rust
.post_connect(|client| Box::pin(async move {
    client.batch_execute("SET application_name = 'web'").await?;
    client.batch_execute("SET heer.node_id = '1'").await?;
    client.batch_execute("SET statement_timeout = '5s'").await?;
    Ok(())
}))
```

### Why fire-on-create-only

If the hook fired on every checkout, it would conflict with
`DjogiContext::set_tenant`'s transaction-local
`set_config('app.tenant_id', $1, true)`. The transaction-local pattern
expects a clean session at the start of every transaction. Per-
checkout reset is intentionally NOT exposed in v0.1.0.

### When the hook errors

A closure that returns `Err` aborts the originating `pool.get()` (or
`with_client` checkout). Deadpool discards the connection and the
caller sees `DjogiError::Db` whose message starts with
`post_connect:`. Hook errors are typically a missing GUC or a
permissions issue — fail loudly at startup rather than silently.

### The closure signature

```rust
F: for<'a> Fn(
        &'a mut tokio_postgres::Client,
    ) -> Pin<Box<dyn Future<Output = Result<(), DjogiError>> + Send + 'a>>
    + Send + Sync + 'static
```

Inline `Box::pin(async move { ... })` blocks satisfy the bound
directly. The closure is stored behind an `Arc`, so a single hook is
shared across all physical connections without per-create
allocation.

---

## Raw-client escape hatch — `with_client`

`pool.with_client(closure)` borrows a `&mut tokio_postgres::Client`
for the closure's lifetime. Use it for operations that cannot route
through `DjogiContext`:

- `COPY FROM STDIN` / `COPY TO STDOUT` and other binary-protocol
  features.
- Server-side cursors driven via the driver API.
- `CREATE EXTENSION` and one-time DDL at cold-start.
- Bridging into third-party crates that take `&tokio_postgres::Client`
  (e.g. `heeranjid::postgres_schema::install_schema`).

```rust
pool.with_client(|client| Box::pin(async move {
    client
        .batch_execute("CREATE EXTENSION IF NOT EXISTS postgis")
        .await?;
    Ok(())
})).await?;
```

### NOT for raw `SELECT` queries

Adopter code that needs a raw query should use
`DjogiContext::raw_query` / `DjogiContext::raw_execute`, which keep
the call inside the framework's pool / transaction substrate, surface
decode helpers, and compose with `atomic()` scopes (so the raw query
participates in the same transaction as the surrounding model
operations). The boundary is tight by design — `with_client` is for
the cases where the framework's path *cannot* express what you need.

### Lifecycle — clean exit returns, dirty exit detaches

This is the safety guarantee: `with_client` is dirty-by-default. The
behaviour on the way out depends on how the closure exits:

- **Clean exit (`Ok`).** The `Object` drops normally and deadpool
  returns the connection to the pool. The next checkout reuses the
  same physical connection.
- **Dirty exit (`Err`, panic, future cancellation).** The `Object` is
  detached via `deadpool::managed::Object::take`, which removes it
  from the pool's tracker; the underlying `ClientWrapper` is dropped
  immediately, closing the `tokio_postgres::Client` and the socket.
  The pool creates a fresh physical connection on the next demand.

The dirty-exit detach is important because Djogi's pool runs
`RecyclingMethod::Fast`, which only checks `is_closed()` on return —
it does NOT run `ROLLBACK`, `RESET ALL`, or `DISCARD ALL`. Without
the detach, a closure that started a transaction or ran `SET ROLE`
would hand its session state to the next request. The trade-off is
one extra physical connection per dirty exit, paid for the safety
guarantee.

### Session-affecting commands on the clean-exit path

Even on the clean-exit path, session-level state set inside the
closure (`SET ROLE`, `SET search_path`, advisory locks, prepared
statements outside the cache) leaves the connection in a non-default
state when it returns to the pool. Prefer transaction-local settings
(`SET LOCAL ...`, `set_config(name, value, true)`,
`BEGIN; ... COMMIT;`) or reset what you set before the closure
resolves.

### The closure signature

```rust
F: for<'a> FnOnce(&'a mut tokio_postgres::Client) -> ClientFuture<'a, R>
```

`ClientFuture<'a, R>` is the public alias for
`Pin<Box<dyn Future<Output = Result<R, DjogiError>> + Send + 'a>>`.
Adopters who factor closure bodies out into named helpers can spell
the lifetime explicitly:

```rust
fn install_extensions<'a>(
    client: &'a mut tokio_postgres::Client,
) -> djogi::pg::pool::ClientFuture<'a, ()> {
    Box::pin(async move {
        client
            .batch_execute("CREATE EXTENSION IF NOT EXISTS postgis")
            .await
            .map_err(djogi::DjogiError::from)
    })
}

pool.with_client(install_extensions).await?;
```

---

## Config-driven entry point — `from_database_config`

`DjogiPool::from_database_config(&cfg.database)` builds a pool whose
`max_size` is resolved from `[database]` configuration plus environment
variables:

1. `DJOGI_DATABASE_MAX_CONNECTIONS` env var — if set and parseable as a
   positive integer.
2. `[database].max_connections` from the loaded `Djogi.toml`, if non-zero.
3. Builder default (`DEFAULT_MAX_SIZE = 5`).

Empty / unparseable / `0` env values fall through to the next layer
rather than zeroing the pool — a typo in the env var must NOT
silently disable the database.

```rust
let cfg = djogi::DjogiConfig::load()?;
let pool = djogi::pg::pool::DjogiPool::from_database_config(&cfg.database).await?;
```

### Combining config-driven sizing with a `post_connect` hook

`from_database_config` does NOT register a `post_connect` hook —
hooks are application-specific. To get both the env > TOML > default
chain AND a hook, use the public `resolve_max_connections` helper:

```rust
use djogi::pg::pool::{DjogiPool, resolve_max_connections};

let cfg = djogi::DjogiConfig::load()?;
let mut b = DjogiPool::builder(&cfg.database.url);
if let Some(n) = resolve_max_connections(&cfg.database) {
    b = b.max_size(n);
}
let pool = b
    .post_connect(|client| Box::pin(async move {
        client.batch_execute("SET application_name = 'web'").await?;
        Ok(())
    }))
    .build()
    .await?;
```

---

## Diagnostics — `pool.status()`

`pool.status()` returns `deadpool_postgres::Status`, a snapshot of
`max_size`, current `size` (physical connections opened), and
`available` (idle connections ready for checkout).

```rust
let s = pool.status();
tracing::info!(
    max_size = s.max_size,
    size = s.size,
    available = s.available,
    "pool snapshot"
);
```

Useful for `/metrics` endpoints, for diagnosing slow checkouts
(`available == 0` and `size == max_size` means you're saturated),
and for integration tests that need to assert pool-state invariants.

`Status` is `Copy`, so the call is a cheap snapshot read — it does
not lock the pool or block on in-flight checkouts.

---

## Backwards compatibility

`DjogiPool::connect(url)` is preserved as sugar for
`DjogiPool::builder(url).build().await`. Every existing call site
keeps compiling against the same defaults (`max_size = 5`, no
timeout, no hook).

Production deployments that previously hard-coded
`DjogiPool::connect` should migrate to either `DjogiPool::builder` or
`DjogiPool::from_database_config` to size against their actual
concurrency budget.

---

## Reference

- `djogi::pg::pool::DjogiPool` — the pool type
- `djogi::pg::pool::DjogiPoolBuilder` — the builder
- `djogi::pg::pool::ClientFuture<'a, R>` — boxed future alias for
  `with_client` closures
- `djogi::pg::pool::resolve_max_connections` — the env > config
  resolver, exposed for adopters who need both the chain and a hook
- `djogi::pg::pool::ENV_DATABASE_MAX_CONNECTIONS` — the env var name
  (`"DJOGI_DATABASE_MAX_CONNECTIONS"`) read by the resolver
- `djogi::pg::pool::DEFAULT_MAX_SIZE` — `5`
- `djogi::DjogiError::PoolTimeout { phase }` — saturation error
  variant; `phase` is `"wait"`, `"create"`, or `"recycle"`

---

## Performance reference

djogi ships smoke benchmarks at `tests/integration/phase8_zero_pool_bench.rs`.
Run

```bash
cargo test --test phase8_zero_pool_bench -p djogi --all-features --release \
    -- --test-threads=1 --nocapture
```

to see throughput on your hardware. The benchmarks are not perf
guarantees — they verify the pool delivers concurrency, the
`post_connect` hook doesn't catastrophically tax connection setup, and
`with_client` overhead is bounded relative to a held-client baseline.
