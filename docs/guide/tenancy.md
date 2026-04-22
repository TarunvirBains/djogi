> [Back to Guides](./index.md) · [Back to README](../../ReadMe.MD)

Spec: [`docs/spec/models.md`](../spec/models.md) — Phase 5 RLS / tenant-key contract.

# Tenancy

Djogi's tenancy support isolates rows at the database level using Postgres
Row-Level Security (RLS). You declare which column is the tenant discriminator;
Djogi emits an RLS policy for the model's table (consumed by Phase 7's migration
differ) and generates `DjogiContext::set_tenant(...)` as the runtime entry point
for activating isolation within a transaction.

Auth — the `DjogiAuth` trait, `EnvAuth`, and web-framework middleware that calls
`set_tenant` automatically on request entry — belongs to Phase 5.5 and is not
yet shipped. The data-layer plumbing (policy emission, `set_tenant`, insecure
bypasses) is available now.

---

## Contract

- You declare `#[model(tenant_key = "col_name")]` on a model. The named column
  must exist as a user-declared field on the struct.
- Djogi emits an RLS policy file at `target/djogi_rls/{table}_rls.sql` — Phase 7
  picks this up during migration generation. Until Phase 7, hand-write the
  policy in your migration:
  ```sql
  ALTER TABLE posts ENABLE ROW LEVEL SECURITY;
  CREATE POLICY tenant_isolation ON posts
      USING (org_id = current_setting('app.tenant_id', true)::bigint);
  ```
- `ctx.set_tenant(tenant_id)` issues
  `SELECT set_config('app.tenant_id', $1, true)` against the current connection,
  scoped to the current transaction (`is_local = true`). The GUC resets when the
  transaction ends.
- `ctx.set_tenant(...)` must be called inside an `atomic()` scope. Called on a
  pool-backed context outside a transaction, the GUC lasts only for the duration
  of the single `set_config` statement, not across subsequent queries on the
  same context.
- `set_tenant` marks an internal flag on the context. Framework code can inspect
  this flag; future phases may use it for RLS-active checks.
- `_insecurely()` suffix methods are generated only on tenant-keyed models. They
  bypass RLS via `SET LOCAL row_security = off` and emit a `tracing::warn!`
  on every call.

---

## Example

```rust
use djogi::prelude::*;

#[model(table = "posts", tenant_key = "org_id")]
#[derive(Debug, Clone)]
pub struct Post {
    pub org_id: i64,
    pub title: String,
    pub body: String,
}

async fn list_posts_for_org(pool: &DjogiPool, org_id: i64) -> Result<Vec<Post>, DjogiError> {
    djogi::transaction::atomic(pool, |ctx| async move {
        // Activate tenant isolation for this transaction.
        ctx.set_tenant(&org_id.to_string()).await?;

        // All queries inside atomic() now see only rows where org_id matches.
        let posts = Post::objects().fetch_all(ctx).await?;
        Ok(posts)
    }).await
}

async fn create_post_for_org(
    pool: &DjogiPool,
    org_id: i64,
    title: String,
    body: String,
) -> Result<Post, DjogiError> {
    djogi::transaction::atomic(pool, |ctx| async move {
        ctx.set_tenant(&org_id.to_string()).await?;

        let post = Post::create(ctx, Post {
            id: HeerId::placeholder(),
            created_at: Default::default(),
            updated_at: Default::default(),
            org_id,
            title,
            body,
        }).await?;

        Ok(post)
    }).await
}
```

---

## Common Patterns

### Admin override using `_insecurely`

Tenant-keyed models gain `_insecurely`-suffixed variants of every CRUD method.
Use them for admin tooling, cross-tenant analytics, or migration scripts that
must see all rows:

```rust
// The bypass only takes effect inside atomic(). objects_insecurely() returns
// a lazy QuerySet; the actual SET LOCAL row_security = off fires only when a
// terminal method (fetch_all, fetch_one, etc.) runs inside a transaction.
// Call ctx.raw_execute("SET LOCAL row_security = off", &[]).await? before
// the terminal method if you need to confirm the bypass is active.
djogi::transaction::atomic(pool, |ctx| async move {
    // Emits: tracing::warn!(model = "Post", method = "objects_insecurely", ...)
    let all_posts = Post::objects_insecurely().fetch_all(ctx).await?;
    Ok(all_posts)
}).await?;

// Single-row bypass (same rule — must be inside atomic()):
djogi::transaction::atomic(pool, |ctx| async move {
    let post = Post::get_insecurely(ctx, post_id).await?;
    Ok(post)
}).await?;
```

Every `_insecurely` terminal call emits a `tracing::warn!` with the model name,
method name, and caller location. This makes audit scanning straightforward:

```text
grep -r _insecurely src/
```

All results are intentional bypasses; accidental use is visible in both the
source search and the runtime warning log.

### Interaction with `FORCE ROW LEVEL SECURITY`

By default, Postgres table owners bypass RLS. To enforce the policy even for
the database role that owns the table (useful in applications where a single
role owns everything), add `FORCE ROW LEVEL SECURITY` to the table:

```sql
ALTER TABLE posts FORCE ROW LEVEL SECURITY;
```

With `FORCE ROW LEVEL SECURITY`, every connection — including application roles —
must satisfy the policy. `set_tenant` activates the policy; an unset GUC causes
Postgres to reject the query (the `current_setting(..., true)` call returns
`NULL`, and `NULL = col` evaluates to `FALSE`, so no rows pass the policy).

### Restricted roles (recommended production setup)

For tighter isolation, create a restricted database role that does not own the
tables and therefore is not a policy bypass candidate:

```sql
CREATE ROLE app_role;
GRANT SELECT, INSERT, UPDATE, DELETE ON posts TO app_role;
```

Connect the application pool as `app_role`. RLS is always active for
non-owner roles; `FORCE ROW LEVEL SECURITY` is only needed for the owner role.

---

## Escape Hatch

When you need to issue queries outside the RLS policy from a context that is
already transaction-bound (for example, reading a lookup table that does not
have tenant isolation), issue `SET LOCAL row_security = off` manually via
`raw_execute`:

```rust
ctx.raw_execute("SET LOCAL row_security = off", &[]).await?;
// Queries here bypass RLS until the transaction commits or rolls back.
let cross_tenant_rows = Post::objects_insecurely().fetch_all(ctx).await?;
// Re-enable before continuing with tenant-isolated work:
ctx.raw_execute("SET LOCAL row_security = on", &[]).await?;
```

This pattern is what the generated `_insecurely` methods automate internally.
Using `raw_execute` directly is appropriate when you need finer-grained control
over which statements bypass the policy within the same transaction.
