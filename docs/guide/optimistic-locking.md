> [Back to Guides](./index.md) · [Back to README](../../README.md)

Spec: [`docs/spec/models.md`](../spec/models.md) — version-field contract.

# Optimistic Locking

Optimistic locking detects write conflicts without holding a row lock for the
duration of a concurrent read-modify-write cycle. You annotate exactly one
integer field with `#[field(version)]`; every `save()` on that model includes a
version predicate in the WHERE clause and a version increment in the SET list.
If another writer has already incremented the version, the UPDATE matches zero
rows and Djogi surfaces `DjogiError::LockConflict`.

---

## Contract

- You annotate one field `#[field(version)] pub <name>: i32` (or `i64`).
 No other type is allowed — `Option<i32>`, `String`, and any other type
 produce a span-precise compile error at the annotated field.
- Two or more `#[field(version)]` annotations on the same model produce a
 compile error on the second occurrence.
- `save()` emits:
 ```sql
 UPDATE <table>
 SET <other_fields>, <version_col> = <version_col> + 1, updated_at = now()
 WHERE id = $n AND <version_col> = $m
 RETURNING *;
 ```
- Zero rows in RETURNING → `Err(DjogiError::LockConflict)`.
- The version field's default value is `0`. The first successful `save()` sets
 it to `1` in the database; the rehydrated row reflects that new value.
- Models without `#[field(version)]` use the baseline UPDATE (no
 version predicate).

---

## Example

```rust
use djogi::prelude::*;
use djogi::error::DjogiError;

#[model(table = "accounts")]
#[derive(Debug, Clone)]
pub struct Account {
 pub owner: String,
 pub balance: i64,
 #[field(version)]
 pub revision: i32,
}

async fn transfer(pool: &DjogiPool, account_id: HeerIdRecencyBiased, amount: i64) -> Result<(), DjogiError> {
 // Writer A loads the account at revision = 3.
 let mut ctx_a = DjogiContext::from_pool(pool.clone());
 let mut account_a = Account::get(&mut ctx_a, account_id).await?;

 // Simulate Writer B concurrently loading and saving the same row first.
 let mut ctx_b = DjogiContext::from_pool(pool.clone());
 let mut account_b = Account::get(&mut ctx_b, account_id).await?;
 account_b.balance -= 50;
 account_b.save(&mut ctx_b).await?; // succeeds; revision becomes 4 in the DB

 // Writer A now tries to save. The DB row is at revision 4 but account_a
 // still holds revision 3. The WHERE clause finds no match.
 account_a.balance += amount;
 match account_a.save(&mut ctx_a).await {
 Ok(()) => {
  // Won the race — revision is now 5 in the DB.
 }
 Err(DjogiError::LockConflict(_)) => {
  // Lost the race — re-read the latest state and retry.
  let fresh = Account::get(&mut ctx_a, account_id).await?;
  // Re-apply the mutation and save again.
  let mut fresh = fresh;
  fresh.balance += amount;
  fresh.save(&mut ctx_a).await?;
 }
 Err(other) => return Err(other),
 }

 Ok(())
}
```

---

## Common Patterns

### Retry loop with `retry_on_conflict`

For code paths that can always safely retry, use the `retry_on_conflict` helper
from the [transactions guide](./transactions.md) instead of writing a manual
match:

```rust
use djogi::transaction::retry_on_conflict;

// ctx is a DjogiContext — obtained from DjogiContext::from_pool or inside atomic().
retry_on_conflict(&mut ctx, 3, async |ctx| {
 let mut account = Account::get(ctx, account_id).await?;
 account.balance += amount;
 account.save(ctx).await?;
 Ok::<_, DjogiError>(())
}).await?;
```

`retry_on_conflict` takes a mutable `DjogiContext` by `&mut`, an attempt count,
and an async closure that receives `&mut DjogiContext`. It retries the entire
closure on any transient lock error (`LockConflict`); the fresh row is re-read
on each attempt naturally because the closure re-executes from the top.

For production paths where the same closure may also hit pool saturation, use
`retry_on_conflict_with_backoff` plus `TransactionRetryBackoff` from the
[transactions guide](./transactions.md). It retries the same transient errors
but sleeps between attempts so `PoolTimeout` does not immediately re-enter a
saturated checkout queue.

### Coupling with `Tracked<T>` for dirty-aware concurrent writes

`Tracked<T>` and `#[field(version)]` compose cleanly. The version predicate
fires on every `save()` call regardless of which `Tracked<T>` fields are dirty.
Use the combination for models where writes are infrequent (version) but field
changes are selective (dirty tracking):

```rust
#[model(table = "profiles")]
pub struct Profile {
 pub display_name: Tracked<String>,
 pub bio: Tracked<String>,
 #[field(version)]
 pub revision: i32,
}
```

### User-visible conflict UI

Not every conflict should be silently retried. For user-facing forms, it is
often better to surface the conflict and ask the user to reconcile:

```rust
match profile.save(&mut ctx).await {
 Ok(()) => redirect_to_profile(),
 Err(DjogiError::LockConflict(_)) => show_conflict_error("Someone else edited this profile — please review and resubmit."),
 Err(e) => return Err(e),
}
```

---

## Escape Hatch

For cases where optimistic locking is too coarse — for example, a high-write
model where version conflicts would be frequent — use Postgres row locks
instead. `select_for_update()` on `QuerySet` acquires a row lock before the
read; no version field is needed:

```rust
djogi::transaction::atomic(&pool, |ctx| Box::pin(async move {
 let mut account = Account::objects()
.filter(|f| f.id().eq(account_id))
.select_for_update()
.fetch_one(ctx).await?;

 account.balance += amount;
 // save() here does not need a version field; the row lock prevents concurrent writes.
 account.save(ctx).await?;
 Ok::<_, DjogiError>(())
})).await?;
```

See the [transactions guide](./transactions.md) for the full row-lock surface.
