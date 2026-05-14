> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# Raw SQL Escape Hatches

Raw SQL in djogi is treated culturally the way `unsafe` is in Rust: not
banned, but always conscious. The typed surface is the default path. Every
reach for raw SQL walks through the verbose attribute and the justification.
Friction is the design.

This spec is the contract for the raw SQL bypass harness. Implementation
commits that change raw SQL, pool access, direct driver access, pin tests, or
the bypass validator must update this file before changing code.

## 1. Forbidden ordinary-test APIs

Ordinary integration tests under `tests/integration/` and
`djogi-cli/tests/integration/` must not call these APIs directly:

- `raw_query`
- `raw_rows`
- `raw_fetch_one`
- `raw_scalar`
- `raw_execute`
- `raw_ddl`
- `raw_stream`
- `raw_stream_with_fetch_size`
- `raw_pool`
- `raw_conn`
- `raw_with_client`
- `pool()`
- `conn()`
- `with_client`
- `batch_execute`
- `__query_all_for_macros`
- `__query_one_for_macros`
- `__query_opt_for_macros`
- `__execute_for_macros`
- direct `tokio_postgres::*` access

This repository's tests also may not manually reference `djogi::__bypass` or
`::djogi::__bypass`. Tests use the bypass attribute so the use site remains
auditable.

## 2. RawAccessExt

The raw methods move from ordinary `DjogiContext` inherent methods onto a
sealed extension trait:

```rust
pub trait RawAccessExt: sealed::Sealed {
    async fn raw_query<T>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<T>, DjogiError>
    where
        T: FromPgRow;

    async fn raw_rows(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<tokio_postgres::Row>, DjogiError>;

    async fn raw_fetch_one<T>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<T, DjogiError>
    where
        T: FromPgRow;

    async fn raw_scalar<T>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<T, DjogiError>
    where
        T: FromSqlOwned;

    async fn raw_execute(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, DjogiError>;

    async fn raw_ddl(&mut self, sql: &str) -> Result<(), DjogiError>;

    async fn raw_stream<'ctx>(
        &'ctx mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<RawCursorStream<'ctx>, DjogiError>;

    async fn raw_stream_with_fetch_size<'ctx>(
        &'ctx mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
        fetch_size: u32,
    ) -> Result<RawCursorStream<'ctx>, DjogiError>;
}
```

Pool-level escape hatches are also extension-trait gated through
`RawPoolAccessExt`, covering `raw_pool`, `raw_conn`, and `raw_with_client`
access needed by pin tests and rare framework-level helpers.

The traits are sealed so downstream crates cannot implement the raw surface
for their own types. The module is public but hidden from rustdoc as
`djogi::__bypass`, giving sibling crates and deliberate adopters a conscious
opt-out path while keeping ordinary docs focused on the typed surface.

## 3. Bypass attribute

Tests and adopter-side helpers unlock raw SQL by decorating the enclosing item:

```rust
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#234): citext column needs case-insensitive equality;
// QuerySet does not expose LOWER(col) equality yet.
async fn my_test(mut ctx: DjogiContext) {
    let rows = ctx.raw_query::<MyRow>("SELECT ...", &[]).await?;
}
```

The macro injects the hidden bypass traits into the decorated item. Adopter
code must not import `djogi::__bypass` directly; use the bypass attribute plus
an adjacent `// JUSTIFICATION ...` comment instead.

```rust
use ::djogi::__bypass::RawAccessExt;
use ::djogi::__bypass::RawPoolAccessExt;
```

When stacked with `#[djogi::djogi_test]`, the bypass attribute is the outer
attribute so the raw imports land in the test body before `djogi_test` rewrites
that body:

```rust
#[djogi::deliberately_bypass_convention_with_raw_sql]
#[djogi::djogi_test(sync_models = [Vehicle])]
// JUSTIFICATION (PIN): exercises raw_query itself.
async fn raw_query_pin(mut ctx: DjogiContext) {
    // ...
}
```

The attribute may decorate functions, inline modules, or impl blocks. It must
not decorate a file-loaded module such as `mod raw_helpers;`; put the attribute
inside that module instead.

## 4. JUSTIFICATION comments

Every bypass attribute under `tests/` must have a syntactically attached
ordinary line comment:

```rust
// JUSTIFICATION (djogi#234): explain the typed-surface gap.
```

The issue number is djogi's tracker, not the adopter application's tracker.
Reaching for raw SQL signals a gap in djogi's typed surface, and that gap
belongs to djogi to fix. If the raw SQL is needed only for an adopter's private
schema, file the narrowest upstream issue that explains the missing framework
capability, or use `JUSTIFICATION (PIN)` only for pin tests that exercise the
raw API itself.

Pin tests use this form:

```rust
// JUSTIFICATION (PIN): exercises raw_execute itself.
```

Attachment is syntactic, not proximity-based. Doc comments and other outer
attributes may sit in the attribute stack, but only ordinary `// JUSTIFICATION`
comments satisfy the harness.

## 5. Pin tests

Pin tests live under `tests/pin/` and are selected as explicit test targets.
They prove the raw APIs themselves still work while keeping ordinary integration
tests on the typed surface. The intended structure is one pin test per raw API:

- `raw_query_pin`
- `raw_rows_pin`
- `raw_fetch_one_pin`
- `raw_scalar_pin`
- `raw_execute_pin`
- `raw_ddl_pin`
- `raw_stream_pin`
- `raw_stream_with_fetch_size_pin`
- a pool-access pin covering `raw_pool` and `raw_conn`

Each pin test uses the bypass attribute and a `JUSTIFICATION (PIN)` comment.

Compile-fail examples live under `tests/compile_fail/raw_sql/`. They prove that
ordinary tests cannot reach raw methods, pool access, or direct driver access
without the sanctioned unlock.

## 6. Xtask validators

The harness has no runtime grep gate. Enforcement is split across the type
system, clippy, and xtask validators.

Local checks:

```bash
cargo xtask check-justifications
cargo xtask check-test-surface
```

`check-justifications` validates bypass attributes under test roots. It rejects
missing `JUSTIFICATION` comments, detached comments, malformed issue forms, and
conditional bypass attributes hidden behind `cfg_attr`.

`check-test-surface` scans ordinary workspace integration-test roots for raw
methods, pool escapes, direct driver calls, and source-level `djogi::__bypass`
references. It does not scan internal framework source or examples.

Raw SQL that is itself the behavior under test lives in explicit internal
framework targets under `tests/internal/` and `djogi-cli/tests/internal/`.
Each raw wrapper module carries the bypass attribute plus a `JUSTIFICATION
(djogi#133)` comment. Ordinary integration roots stay on typed/public Djogi
APIs and remain inside `check-test-surface`'s zero-raw ratchet.

## 7. Workspace raw callers

Internal framework callers do not use the bypass attribute. They import the
hidden extension traits explicitly and keep the raw dependency visible at the
top of the module:

```rust
use crate::__bypass::RawAccessExt as DjogiRawAccessExt;
use crate::__bypass::RawPoolAccessExt as DjogiRawPoolAccessExt;
```

Sibling workspace crates and examples use the public hidden path:

```rust
use djogi::__bypass::{
    RawAccessExt as DjogiRawAccessExt,
    RawPoolAccessExt as DjogiRawPoolAccessExt,
};
```

Aliases are preferred because they avoid collisions with local trait names.
These internal and example imports are deliberate raw callers, not ordinary
integration-test bypasses, so they do not require `JUSTIFICATION` comments.

## 8. Migration philosophy

Raw SQL is not banned. It is deliberately loud. Djogi will not ship a fluent
`ctx.raw().execute(...)` shortcut or a `RawSqlBuilder`. Ordinary tests exercise
`Model::create`, `Model::save`, `Model::delete`, `Model::objects()`,
`djogi::transaction::atomic`, and `#[djogi::djogi_test(sync_models = [...])]`.

Every escape hatch must make reviewers ask why the typed surface is not enough.
That question is the point of the harness.

## 9. Connection lifecycle — dirty-by-default

The pool-backed raw methods on `RawAccessExt` (`raw_query`, `raw_rows`,
`raw_fetch_one`, `raw_scalar`, `raw_execute`, `raw_ddl`) acquire a pooled
connection through the framework's execution helpers. Each pool checkout is
wrapped in a dirty-by-default guard that mirrors `DjogiPool::with_client`:

- **Clean exit (`Ok`).** The connection returns to the pool the normal way;
  the next checkout reuses it.
- **Dirty exit (`Err`, panic, future cancellation).** The connection is
  detached via `deadpool_postgres::Object::take` and dropped immediately.
  The pool will create a fresh physical connection on the next demand.

This is required because djogi runs its pools with
`deadpool_postgres::RecyclingMethod::Fast`, which only checks `is_closed()`
on return — it does NOT issue `ROLLBACK`, `RESET ALL`, or `DISCARD ALL`.
Without the dirty-exit detach, a bypassed `raw_execute` that runs
`SET ROLE`, `SET search_path`, advisory lock acquisition, manual
`BEGIN`/`COMMIT`, `LISTEN`/`UNLISTEN`, or any other session-state mutation
and then errors or panics would leak that state to the next checkout — a
real auth/tenant/session-state hazard for multi-tenant deployments.

The trade-off is one extra physical connection per dirty exit. This is the
right cost to pay for the guarantee.

### Post-query decode dirty-exit

`raw_query`, `raw_fetch_one`, and `raw_scalar` perform framework-side
decoding after the underlying SQL returns. A pathological mix — server-side
mutation in the SQL plus a column type the caller's `T` cannot decode — can
produce an SQL `Ok` paired with a decode `Err`. Example:

```rust,ignore
let _: i32 = ctx
    .raw_scalar("SELECT set_config('application_name', 'poisoned', false)", &[])
    .await?;
```

The SQL succeeds and mutates the session GUC; `set_config` returns text, so
`try_get_scalar::<i32>` then fails. The decode is routed through the same
pool guard via `query_all_with` / `query_opt_with`, so the `Err` arms the
guard for detach and the poisoned connection is closed before the next
checkout sees it. Adopters do not need to reason about this case
explicitly — it is covered by the dirty-by-default lifecycle.

### Adopter contract

The dirty-by-default guard fires on `Err` / panic / cancellation paths
only. On the **clean-exit path**, session state mutated by an `Ok` raw
call still leaves the connection non-default when it returns to the
pool. Adopters who run session-state-affecting raw SQL must wrap the
call in `djogi::transaction::atomic(...)` **and** either:

- use a TRANSACTION-LOCAL form so a `COMMIT` or `ROLLBACK` clears the
  state automatically — `SET LOCAL key = value` instead of `SET key =
  value`, `set_config(name, value, true)` instead of
  `set_config(name, value, false)`, `pg_advisory_xact_lock(...)` instead
  of `pg_advisory_lock(...)`, etc.; or
- explicitly reset / unlock / `UNLISTEN` / `DEALLOCATE` the
  session-level mutation on **every non-cancel exit** of the closure —
  before returning `Ok`, in every error branch, and in any panic
  recovery. `atomic()` will NOT do this cleanup for you on `Err` /
  panic; the next paragraph explains why.

`atomic()` is a transaction guard, not a session-state reset guard. Its
`ROLLBACK` path on `Err` / panic only unwinds TRANSACTION-SCOPED state
(row writes, sequence allocations, `SET LOCAL`,
`set_config(_, _, true)`, `pg_advisory_xact_lock`). SESSION-scoped
state survives both clean `COMMIT` and `ROLLBACK`: session advisory
locks explicitly ignore transaction rollback per Postgres semantics,
plain `SET` / `SET ROLE` / `SET search_path` are reset by `ROLLBACK`
only when the same transaction issued them, and `LISTEN` / prepared
statements bypass transactional rollback entirely.

Two worked examples:

- A `SET search_path = 'audit'` inside an `atomic()` closure that
  returns `Ok` commits but the new `search_path` survives `COMMIT` and
  rides the connection back to the pool.
- A `pg_advisory_lock(424242)` acquired inside an `atomic()` closure
  that subsequently returns `Err` is NOT released by `ROLLBACK`; the
  next pool checkout can still see the lock held and would have to call
  `pg_advisory_unlock(424242)` (which `atomic()` will not do for you).

Adopters must choose transaction-local forms or run explicit reset on
every non-cancel exit for the contract to hold.

#### `atomic()` cancellation caveat

`atomic()` issues `ROLLBACK` on the closure's `Err` and panic paths. It
does NOT issue `ROLLBACK` when the entire `atomic()` future is dropped
mid-execution (e.g. wrapping an `atomic` call in `tokio::time::timeout` and
having the timeout fire before the closure resolves). In that case the
transaction-backed `DjogiContext` drops without async cleanup and the
underlying connection returns to the pool with the transaction still open.

This is a pre-existing transaction-scope hazard tracked separately from
djogi#162. It is not introduced by the pool-path guard described above,
but adopters relying on `atomic()` as a session-state isolation mechanism
should avoid wrapping it in cancellation primitives until the gap is
closed.

Cursors, `COPY` streams, and other multi-round-trip protocol operations
should go through `RawPoolAccessExt::raw_with_client`. Its `WithClientGuard`
bounds the protocol exchange to a single checkout and applies the same
dirty-detach on dirty exit.

Tracking issue: [djogi#162](https://github.com/TarunvirBains/djogi/issues/162).
