> [Back to Guides](./index.md) · [Back to README](../../ReadMe.MD)

Spec: [`docs/spec/models.md`](../spec/models.md) — Phase 5 RLS / tenant-key contract.

# Tenancy

Djogi's tenancy support isolates rows at the database level using Postgres
Row-Level Security (RLS). You declare which column is the tenant discriminator;
Djogi emits an RLS policy for the model's table (consumed by Phase 7's migration
differ) and generates `DjogiContext::set_tenant(...)` as the runtime entry point
for activating isolation within a transaction.

The auth substrate is shipped: `DjogiAuth`, `AuthContext`, and the model-aware
auto-`set_tenant` path activate tenant isolation when a context carries a
tenant id and the queried model is tenant-keyed. Framework middleware and
provider adapters remain application-layer concerns unless a later adapter crate
adds them.

---

## Contract

- You declare `#[model(tenant_key = "col_name")]` on a model. The named column
  must exist as a user-declared field on the struct.
- Djogi's descriptor-driven migration flow includes the tenant RLS policy for
  the model. Use `djogi migrations compose` for generated migration
   plans and `djogi migrations apply` to run them (or the public
   `djogi::migrate` library APIs when applying from code). The `rollback`/`fake`/`baseline`/`repair`/`verify` CLI dispatchers are deferred; hand-written RLS
  SQL is an escape hatch, not the default path. The emitted policy is equivalent
  to:
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
- `_insecurely()` suffix methods are generated only on tenant-keyed models and
  emit a `tracing::warn!` on every call. The async variants (`get_insecurely`,
  `create_insecurely`, `save_insecurely`, `delete_insecurely`,
  `bulk_*_insecurely`) bypass RLS by issuing `SET LOCAL row_security = off`
  themselves before running their query. `objects_insecurely()` is the
  exception: it is synchronous with no ctx, so it cannot issue `SET LOCAL` —
  the caller must issue the bypass on the ctx before the terminal method (see
  the `objects_insecurely` pattern below).

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
    djogi::transaction::atomic(pool, |ctx| Box::pin(async move {
        // Activate tenant isolation for this transaction.
        ctx.set_tenant(&org_id.to_string()).await?;

        // All queries inside atomic() now see only rows where org_id matches.
        let posts = Post::objects().fetch_all(ctx).await?;
        Ok(posts)
    })).await
}

async fn create_post_for_org(
    pool: &DjogiPool,
    org_id: i64,
    title: String,
    body: String,
) -> Result<Post, DjogiError> {
    djogi::transaction::atomic(pool, |ctx| Box::pin(async move {
        ctx.set_tenant(&org_id.to_string()).await?;

        let post = Post::create(ctx, Post {
            org_id,
            title,
            body,
            ..Default::default()
        }).await?;

        Ok(post)
    })).await
}
```

---

## Common Patterns

### Admin override using `_insecurely`

Tenant-keyed models gain `_insecurely`-suffixed variants of every CRUD method.
Use them for admin tooling, cross-tenant analytics, or migration scripts that
must see all rows:

The async `_insecurely` methods (`get_insecurely`, `create_insecurely`,
`save_insecurely`, `delete_insecurely`, `bulk_*_insecurely`) issue
`SET LOCAL row_security = off` themselves, so they deliver the bypass as long
as the ctx is inside an `atomic()` scope (`SET LOCAL` outside a transaction
silently no-ops). `objects_insecurely()` is different: it is a synchronous
method with no ctx, so it cannot issue `SET LOCAL` — it only logs the warn
and hands back `Model::objects()`. To bypass RLS on a queryset fetch, the
caller has to issue the `SET LOCAL` on the ctx before the terminal method.

```rust
use djogi::prelude::*;

// Async method — bypass is internal; just wrap the call in atomic():
async fn fetch_one(pool: &DjogiPool, post_id: HeerIdRecencyBiased) -> djogi::Result<Post> {
    djogi::transaction::atomic(pool, |ctx| Box::pin(async move {
        let post = Post::get_insecurely(ctx, post_id).await?;
        Ok(post)
    })).await
}

// Lazy queryset — caller issues SET LOCAL before the terminal method.
// `ctx.raw_execute` requires the bypass attribute + JUSTIFICATION because raw
// SQL is djogi's `unsafe`-equivalent (see ../spec/raw-sql-escape-hatches.md).
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#234): `SET LOCAL row_security` is a session GUC; no typed surface covers it.
async fn fetch_all_insecurely(pool: &DjogiPool) -> djogi::Result<Vec<Post>> {
    djogi::transaction::atomic(pool, |ctx| Box::pin(async move {
        // Emits: tracing::warn!(model = "Post", method = "objects_insecurely", ...)
        let qs = Post::objects_insecurely();
        ctx.raw_execute("SET LOCAL row_security = off", &[]).await?;
        let all_posts = qs.fetch_all(ctx).await?;
        Ok(all_posts)
    })).await
}
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

> **Note on superusers.** `FORCE ROW LEVEL SECURITY` covers the **owner**
> bypass but does NOT cover Postgres **superusers** — superuser bypass is a
> Postgres-level guarantee, not a djogi one. If your test harness connects
> as a superuser (common when the test-DB owner is also a cluster superuser
> in local dev / CI environments), RLS is silently bypassed regardless of
> `FORCE ROW LEVEL SECURITY`, and tenant-isolation tests can pass with all
> rows visible — a false-positive green.
>
> Two safe paths:
>
> - **Production:** connect the application pool as a non-owner, non-superuser
>   role per the "Restricted roles" subsection below. RLS is always active for
>   such roles.
> - **Test harness:** if connecting as the owner is unavoidable, drop privilege
>   inside the test scope with `SET LOCAL ROLE <restricted_role>` issued via
>   `ctx.raw_execute(...)` before the `set_tenant(...)` call. The `LOCAL`
>   keyword scopes the role change to the current transaction, so it
>   automatically reverts at commit / rollback.

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
`raw_execute`. `ctx.raw_execute` is part of djogi's raw escape surface, so
the enclosing item must be decorated with
`#[djogi::deliberately_bypass_convention_with_raw_sql]` and paired with an
adjacent `// JUSTIFICATION (djogi#<n>): ...` comment (see
[Raw SQL escape hatches](../spec/raw-sql-escape-hatches.md)):

```rust
use djogi::prelude::*;

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#234): SET LOCAL row_security toggles a session GUC; no typed surface covers it.
async fn cross_tenant_window(ctx: &mut DjogiContext) -> djogi::Result<Vec<Post>> {
    ctx.raw_execute("SET LOCAL row_security = off", &[]).await?;
    // Queries here bypass RLS until the transaction commits or rolls back.
    let cross_tenant_rows = Post::objects_insecurely().fetch_all(ctx).await?;
    // Re-enable before continuing with tenant-isolated work:
    ctx.raw_execute("SET LOCAL row_security = on", &[]).await?;
    Ok(cross_tenant_rows)
}
```

This pattern is what the generated `_insecurely` methods automate internally.
Using `raw_execute` directly is appropriate when you need finer-grained control
over which statements bypass the policy within the same transaction.
