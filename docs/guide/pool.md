> [Back to README](../../README.md) | [All Guides](./index.md)

# Connection Pool — DjogiPool

`DjogiPool` is the framework's Postgres connection pool. It wraps
`deadpool_postgres::Pool` with a Djogi-specific builder, a
`post_connect` hook for per-physical-connection setup, and a
config-driven entry point that walks `env > Djogi.toml > builder
default` for sizing.

For the rare cases where a closure needs a raw `&mut
tokio_postgres::Client` — `COPY FROM STDIN` / `COPY TO STDOUT`,
one-time DDL like `CREATE EXTENSION` at cold-start, or bridging
into third-party crates that take `&tokio_postgres::Client`
directly — the pool exposes an explicit raw-driver bypass through
the sealed [`RawPoolAccessExt::raw_with_client`](#raw-client-escape-hatch--raw_with_client)
trait. This is the same opt-in bypass pattern Djogi uses for
`raw_query` / `raw_execute` on `DjogiContext`; see the
[raw SQL escape hatches spec](../spec/raw-sql-escape-hatches.md)
for the broader contract.

This guide covers the `DjogiPool` public surface — the builder API,
the `post_connect` hook for per-physical-connection setup, and the
`raw_with_client` raw-driver bypass. For the broader context —
`DjogiContext`, transactions, raw queries — see the
[Transactions guide](./transactions.md) and the
[Getting Started guide](./getting-started.md).

> **Status — public pool surface.** `DjogiPool::builder`,
> `DjogiPool::status`, and the explicit
> `RawPoolAccessExt::raw_with_client` bypass are the supported public
> surface for Phase 8.5. COPY, server-side cursors, cold-start DDL, and
> third-party direct-driver integrations intentionally stay behind the
> raw bypass; no separate typed COPY/streaming wrapper is part of this
> surface.

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

- `max_size = 5` (`djogi::pg::pool::DEFAULT_MAX_SIZE`)
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
    client.batch_execute("SET heer.ranj_node_id = '1'").await?;
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
`raw_with_client` checkout). Deadpool discards the connection and the
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

## Raw-client escape hatch — `raw_with_client`

`pool.raw_with_client(closure)` borrows a `&mut tokio_postgres::Client`
for the closure's lifetime. It is the **explicit raw-driver bypass**
on the pool: the inherent `DjogiPool::with_client` method is
`pub(crate)` (internal substrate uses it directly), and adopter code
reaches the same behaviour through the sealed
[`RawPoolAccessExt::raw_with_client`](../spec/raw-sql-escape-hatches.md)
trait. Do not import `djogi::__bypass` directly; decorate the enclosing item
with `#[djogi::deliberately_bypass_convention_with_raw_sql]` and an adjacent
`// JUSTIFICATION ...` comment so the bypass macro injects the hidden trait.

Use `raw_with_client` for operations that genuinely cannot route
through `DjogiContext`:

- `COPY FROM STDIN` / `COPY TO STDOUT` and other binary-protocol
  features.
- Server-side cursors driven via the driver API.
- `CREATE EXTENSION` and other one-time DDL at cold-start /
  bootstrap.
- Bridging into third-party crates that take a
  `&tokio_postgres::Client` directly (e.g. installing HeeRanjID's
  schema, third-party migration helpers).

```rust
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#234): one-time extension bootstrap requires direct driver DDL.
async fn install_postgis(pool: &DjogiPool) -> djogi::Result<()> {
    pool.raw_with_client(|client| Box::pin(async move {
        client
            .batch_execute("CREATE EXTENSION IF NOT EXISTS postgis")
            .await?;
        Ok(())
    })).await?;
    Ok(())
}
```

COPY uses the same public bypass. Keep the full protocol exchange inside
the closure so the pool guard can return the connection on success or
detach it on error/cancellation:

```rust
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#66): COPY IN needs tokio-postgres' binary protocol.
async fn copy_payloads(pool: &DjogiPool) -> djogi::Result<()> {
    pool.raw_with_client(|client| Box::pin(async move {
        let sink = client
            .copy_in("COPY payloads (id, body) FROM STDIN BINARY")
            .await?;
        let writer = tokio_postgres::binary_copy::BinaryCopyInWriter::new(
            sink,
            &[
                tokio_postgres::types::Type::INT4,
                tokio_postgres::types::Type::BYTEA,
            ],
        );
        tokio::pin!(writer);
        let body: &[u8] = b"example payload";
        writer.as_mut().write(&[&1_i32, &body]).await?;
        writer.as_mut().finish().await?;
        Ok(())
    })).await
}
```

The `djogi::__bypass` path is intentionally `#[doc(hidden)]` and
sealed — it is public so workspace examples and adopter crates can
opt in consciously, but it is hidden from rustdoc so the typed
surface stays the obvious default. See the
[raw SQL escape hatches spec](../spec/raw-sql-escape-hatches.md)
for the full bypass contract (sealed traits, the
`#[deliberately_bypass_convention_with_raw_sql]` attribute used in
tests, and the `JUSTIFICATION` comment convention).

### NOT for raw `SELECT` queries

Adopter code that needs a raw query should use the
`RawAccessExt::raw_query` / `RawAccessExt::raw_execute` bypass on
`DjogiContext`, which keeps the call inside the framework's pool /
transaction substrate, surfaces decode helpers, and composes with
`atomic()` scopes (so the raw query participates in the same
transaction as the surrounding model operations). The boundary is
tight by design — `raw_with_client` is for the cases where the
framework's path *cannot* express what you need (binary protocol,
cold-start DDL, third-party `&tokio_postgres::Client` bridges), not
for routine SELECTs.

### Lifecycle — clean exit returns, dirty exit detaches

This is the safety guarantee: `raw_with_client` is dirty-by-default.
The behaviour on the way out depends on how the closure exits:

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

pool.raw_with_client(install_extensions).await?;
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

`pool.status()` returns `DjogiPoolStatus`, a Djogi-owned snapshot of
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

`DjogiPoolStatus` is `Copy`, so the call is a cheap snapshot read — it does
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

That `post_connect` block is a single-node example. The pool does NOT read
`HEER_NODE_ID` automatically — node identity is caller-owned through explicit
`post_connect` wiring. For multi-node deployments, register each node in
HeeRanjID first, then set both HeeRanjID session GUCs from the deployment-selected
value in your `post_connect` hook. Migration-runner CLI commands have a separate
identity resolver (`--node-id` / `HEER_NODE_ID` at the CLI boundary only);
runtime application pools remain caller-owned.

See https://github.com/TarunvirBains/heeranjid-sql/blob/main/README.md and
https://github.com/TarunvirBains/HeeRanjID/issues/49.

---

## Reference

- `djogi::pg::pool::DjogiPool` — the pool type
- `djogi::pg::pool::DjogiPoolBuilder` — the builder
- `djogi::pg::pool::ClientFuture<'a, R>` — boxed future alias for
  `raw_with_client` closures
- `djogi::pg::pool::resolve_max_connections` — the env > config
  resolver, exposed for adopters who need both the chain and a hook
- `djogi::pg::pool::ENV_DATABASE_MAX_CONNECTIONS` — the env var name
  (`"DJOGI_DATABASE_MAX_CONNECTIONS"`) read by the resolver
- `djogi::pg::pool::DEFAULT_MAX_SIZE` — `5`
- `djogi::__bypass::RawPoolAccessExt` — sealed bypass trait that
  exposes `raw_with_client` (and `raw_pool` / `raw_conn`) on
  `DjogiPool` and `DjogiContext`. The trait module is `#[doc(hidden)]`;
  see [raw SQL escape hatches](../spec/raw-sql-escape-hatches.md).
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
`raw_with_client` overhead is bounded relative to a held-client baseline.
