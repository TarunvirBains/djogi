> [Back to README](../../ReadMe.MD) | [All Guides](./index.md)

# Hooks and Composition

Djogi ships two complementary systems for attaching cross-cutting
behaviour to model CRUD operations:

- **`#[model(hooks)]` + `ModelHooks`** — per-model lifecycle callbacks
  that fire at precise points in the `before → DB → outbox → after`
  sequence, with `on_commit` callbacks draining at transaction commit
  (requires an `atomic()` context — see below).
- **`#[model(auditable)]` / `#[model(soft_deletable)]`** — one-line
  opt-ins for common composition patterns (who-created-this, logical
  delete) that integrate with the hook sequence without requiring the
  adopter to implement every method.

Both are Phase 8 features. They compose orthogonally — a model can carry
any combination of `hooks`, `auditable`, and `soft_deletable`.

---

## Lifecycle hooks — `#[model(hooks)]` and `ModelHooks`

### Opt in

Add `hooks` to the `#[model(...)]` attribute:

```rust
use djogi::prelude::*;

#[model(table = "posts", hooks)]
#[derive(Debug, Clone)]
pub struct Post {
    pub title: String,
    pub body: String,
    pub slug: String,
}
```

Then implement `ModelHooks` for the struct:

```rust
impl djogi::hooks::ModelHooks for Post {
    async fn before_create(
        &mut self,
        _ctx: &mut djogi::DjogiContext,
    ) -> Result<(), djogi::DjogiError> {
        // Normalise the slug before INSERT.
        self.slug = self.title.to_lowercase().replace(' ', "-");
        Ok(())
    }

    async fn after_create(
        &self,
        ctx: &mut djogi::DjogiContext,
    ) -> Result<(), djogi::DjogiError> {
        // Queue a search-index update to fire after commit.
        ctx.on_commit(|| async { /* … */ Ok(()) });
        Ok(())
    }
}
```

A model with `#[model(hooks)]` but no `impl ModelHooks` fails to compile —
the type system enforces the contract.

A model **without** `#[model(hooks)]` has zero hook-dispatch overhead at
monomorphisation time — LLVM elides the dead branches regardless of LTO
settings.

### The six methods

All six methods default to `async { Ok(()) }`. Override only the ones you
need.

| Method | Receiver | Typical use |
|---|---|---|
| `before_create` | `&mut self` | Normalise derived fields (slug, search vector), set audit columns, validate pre-conditions |
| `after_create` | `&self` | Queue `on_commit` side effects, emit audit events |
| `before_save` | `&mut self` | Re-normalise changed fields, validate invariants |
| `after_save` | `&self` | Queue downstream sync, update denormalised aggregates |
| `before_delete` | `&mut self` | Guard against disallowed deletes, cascade soft-deletes manually |
| `after_delete` | `&self` | Queue cleanup jobs, emit audit trail entries |

`before_*` methods take `&mut self` so the body can mutate the model
before the SQL write. `after_*` methods take `&self` — the row is already
persisted.

Every method receives `&mut DjogiContext` so the hook body inherits the
surrounding tenant scope, `AuthContext`, and the `on_commit` queue.

### Sequencing and error semantics

The canonical execution sequence for `Model::create` **inside an `atomic()` block** is:

```
auto_set_tenant
composition populators  ← auditable, soft_deletable (if opt-in)
before_create           ← ModelHooks (if #[model(hooks)])
INSERT … RETURNING
outbox submission
after_create            ← ModelHooks (if #[model(hooks)])
[on_commit drain]       ← fires when the surrounding atomic() commits
```

`Model::save` follows the same pattern with `before_save` / `after_save`;
`Model::delete` uses `before_delete` / `after_delete`.

> **`on_commit` requires a transaction-backed context.**
> `ctx.on_commit(...)` registers callbacks to run after the current
> transaction commits. When the surrounding `DjogiContext` is pool-backed
> (i.e. the operation is called outside any `atomic()` block), callbacks
> registered via `on_commit` are **warned and dropped** — they never fire.
> To reliably drain `on_commit` callbacks, wrap the operation in
> `djogi::transaction::atomic(...)`:
>
> ```rust
> djogi::transaction::atomic(&mut ctx, |ctx| Box::pin(async move {
>     let post = Post::create(ctx, Post { title: "Hello".into(), ..Default::default() }).await?;
>     // after_create queued ctx.on_commit(...) above — it will drain when
>     // this atomic block commits.
>     Ok(post)
> })).await?;
> ```
>
> See [Transactions](./transactions.md) for the full `on_commit` API and
> nesting semantics.

Returning `Err` from any hook propagates via `?` and no `after_*` hook
fires. The transaction-safety guarantee depends on how the operation is
called:

- **Inside `atomic()`** — returning `Err` rolls back every write in the
  surrounding transaction, including any writes made earlier in the same
  `atomic` block. The partial state is never visible to other connections.
- **Outside `atomic()` (autocommit)** — each SQL statement commits
  individually. Returning `Err` from a `before_*` hook prevents the
  _current_ SQL write (INSERT / UPDATE / DELETE) from being issued, but
  any earlier autocommitted writes in the same call chain are not
  rolled back.

As a result, error-checking in a `before_*` hook is reliable only for the
write that hook gates, not for writes that already committed. When you need
all-or-nothing semantics across multiple writes, wrap the entire operation
in `djogi::transaction::atomic(...)`.

---

## `#[model(auditable)]` — who-created-this

Add `auditable` to opt a model into automatic `created_by` population:

```rust
#[model(table = "posts", auditable)]
#[derive(Debug, Clone)]
pub struct Post {
    pub title: String,
    pub created_by: Option<String>,   // adopter declares the field
}
```

The macro emits:

1. `impl djogi::Auditable for Post` — provides `created_by(&self) ->
   Option<&str>`.
2. A `__djogi_auditable_populate(&mut self, ctx)` helper that runs before
   the user `before_create` hook. It reads `ctx.auth()?.user_id` (via
   `Display`) and sets `self.created_by` when the field is currently
   `None`. If `ctx.auth()` is absent (seeds, migrations, framework-
   internal paths), `created_by` stays `None` — no warning is emitted.

The `if is_none()` guard means a value set by the adopter before calling
`create` is **preserved** — `auditable` only fills in the gap, it does not
overwrite.

### Composition with `hooks`

The composition populator runs BEFORE any user `ModelHooks::before_create`,
so the `before_create` body already sees the populated `created_by` and
can inspect or override it:

```rust
#[model(table = "posts", auditable, hooks)]
#[derive(Debug, Clone)]
pub struct Post {
    pub title: String,
    pub created_by: Option<String>,
}

impl djogi::hooks::ModelHooks for Post {
    async fn before_create(&mut self, _ctx: &mut djogi::DjogiContext)
        -> Result<(), djogi::DjogiError>
    {
        // self.created_by is already populated here (or None for auth-less contexts).
        if self.created_by.is_none() {
            return Err(djogi::DjogiError::Validation("created_by is required".into()));
        }
        Ok(())
    }
}
```

---

## `#[model(soft_deletable)]` and `.not_deleted()`

Add `soft_deletable` to opt a model into logical-delete tracking:

```rust
#[model(table = "posts", soft_deletable)]
#[derive(Debug, Clone)]
pub struct Post {
    pub title: String,
    pub deleted_at: Option<djogi::DateTime>,  // adopter declares the field
}
```

The macro emits `impl djogi::SoftDeletable for Post` — providing access to
`deleted_at(&self) -> Option<DateTime>` and the `COLUMN` const
(`"deleted_at"` by default).

### `.not_deleted()` filter

`QuerySet::not_deleted()` is a convenience method that adds a
`deleted_at IS NULL` filter to the queryset. The column name is read
through `<M as SoftDeletable>::COLUMN` so a future column-rename override
takes effect automatically:

```rust
// Return only live (non-soft-deleted) posts.
let posts = Post::objects()
    .not_deleted()
    .fetch_all(&mut ctx)
    .await?;
// WHERE deleted_at IS NULL
```

> **Note: automatic default-filter is deferred.** In v0.1.0, `objects()`
> does **not** automatically exclude soft-deleted rows — you must call
> `.not_deleted()` explicitly on every queryset where you want the filter.
> Automatic composition (making `objects()` exclude soft-deleted rows by
> default with an `_insecurely()` bypass) is planned for a later phase
> alongside the `Q<T>` substrate.

### Soft deletes in practice

`SoftDeletable` does not hook into `Model::delete`. A soft delete requires
setting `deleted_at` manually and calling `save()`:

```rust
let mut post = Post::objects().filter(|f| f.id().eq(id)).fetch_one(&mut ctx).await?;
post.deleted_at = Some(djogi::DateTime::now_utc());
post.save(&mut ctx).await?;
```

### Soft-delete in practice: the explicit update path

The reliable pattern for soft deletion is the explicit update path shown
above — set `deleted_at` and call `save()`. Do this at every call site that
would otherwise call `delete()`, or centralise it in a domain method:

```rust
impl Post {
    pub async fn soft_delete(&mut self, ctx: &mut djogi::DjogiContext)
        -> Result<(), djogi::DjogiError>
    {
        self.deleted_at = Some(djogi::DateTime::now_utc());
        self.save(ctx).await
    }
}
```

> **Why not intercept `Model::delete` via `before_delete`?**
>
> A `before_delete` hook that calls `save()` then returns `Err` looks
> like it redirects hard-delete to soft-delete, but the semantics are
> unreliable **outside** an `atomic()` transaction. Because `save()`
> autocommits when called in an autocommit context, the soft-delete
> timestamp is durably written even if the hook then returns `Err`. The
> caller receives an error but the row has already been marked deleted —
> contradictory behaviour. Inside `atomic()` the rollback semantics are
> correct (both the `save()` and the aborted `DELETE` are rolled back on
> `Err`), but the pattern still surprises callers who expect
> `post.delete(&mut ctx).await?` to succeed on a soft-deletable model.
>
> Use the explicit `soft_delete` domain method above and do not hook
> `before_delete` for the purpose of intercepting hard deletes. Reserve
> `before_delete` for genuine pre-condition guards (e.g. "refuse delete
> if the post has published comments") where returning `Err` without any
> prior write is safe.

---

## Composition metadata in `FieldDescriptor`

When `auditable` or `soft_deletable` are opted in, the macro marks the
composition-contributed field with a `composed_via` tag on its
`FieldDescriptor`:

- **`FieldDescriptor::composed_via: Option<&'static str>`** — set to
  `Some("Auditable")` on the `created_by` field when `#[model(auditable)]`
  is active; set to `Some("SoftDeletable")` on the `deleted_at` field when
  `#[model(soft_deletable)]` is active. All other fields carry
  `composed_via: None`.

This tag lets `djogi docs` (and admin tooling such as `djogi-maahi`) identify
which fields originate from composition opt-ins versus adopter-declared
domain fields, so generated reference pages can annotate the provenance
correctly.

**Hooks presence is not recorded in the descriptor.** Whether a model
implements `ModelHooks` is witnessed at compile time by the sealed `HasHooks`
marker trait — the framework does not expose a runtime `bool` flag for hook
registration, because hook dispatch is monomorphised away entirely at link
time for models without `#[model(hooks)]`.

---

## Combining all three

```rust
#[model(table = "posts", auditable, soft_deletable, hooks)]
#[derive(Debug, Clone)]
pub struct Post {
    pub title: String,
    pub created_by: Option<String>,
    pub deleted_at: Option<djogi::DateTime>,
}

impl djogi::hooks::ModelHooks for Post {
    async fn before_create(&mut self, _ctx: &mut djogi::DjogiContext)
        -> Result<(), djogi::DjogiError>
    {
        // created_by already populated by auditable populator.
        Ok(())
    }
}
```

The three opt-ins produce independent macro-emitted impl blocks. They do
not interact except through the sequencing rule (composition populators
fire before user hooks).

---

## See also

- [Models](./models.md) — `#[model(...)]` attribute reference
- [Proxy Models](./proxy.md) — per-type hooks on proxy slices
- [Transactions](./transactions.md) — `on_commit` queue, `atomic()`, savepoints
- [Authentication](./auth.md) — `AuthContext` and `ctx.auth()` used by the
  auditable populator
