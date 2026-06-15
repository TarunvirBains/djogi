# Authentication

> [Back to index](./index.md) · [Back to README](../../README.md)

Djogi ships a **narrow authentication substrate** — typed primitives you plug your own provider into, not a full session platform. gives you:

- A pluggable [`DjogiAuth`](#the-djogiauth-trait) trait (not sealed — third-party providers are first-class).
- An [`AuthContext`](#authcontext) value type attached to [`DjogiContext`](./transactions.md).
- [`PasswordHash`](#passwordhash) typed column behind the `auth-argon2` feature flag.
- [Auto-`set_tenant`](#auto-set_tenant-integration) when `ctx.auth().tenant_id` is set and the model is tenant-keyed.
- `_insecurely` bypass variants that emit a grep-able `tracing::warn!` on every call.

Explicitly **not** in scope: session stores, JWT / OIDC providers, rate limiting, MFA, cookie lifetime management, Axum extractor helpers. Those live in your application layer or a later adapter phase.

## The `DjogiAuth` trait

```rust
use djogi::auth::{AuthContext, AuthError, DjogiAuth};
use std::future::Future;
use std::pin::Pin;

pub trait DjogiAuth: Send + Sync + 'static {
 fn authenticate<'a>(
 &'a self,
 token: &'a str,
 ) -> Pin<Box<dyn Future<Output = Result<AuthContext, AuthError>> + Send + 'a>>;

 fn verify<'a>(
 &'a self,
 ctx: &'a AuthContext,
 action: &'a dyn std::any::Any,
 ) -> Pin<Box<dyn Future<Output = Result<(), AuthError>> + Send + 'a>>;
}
```

Both methods are required — there is no default body for `verify`. A provider that wants authenticate-only semantics must explicitly return `Ok(())` from `verify`, making the choice visible at the implementation site; the framework refuses to teach fail-open authorization. Every other implementation should fail closed (typically `Err(AuthError::Denied {.. })`) unless an explicit policy authorises the resolved `AuthContext` for the supplied `action`. Both methods return boxed futures so the trait is **object-safe** — `Arc<dyn DjogiAuth>` works at runtime. The `action: &dyn Any` parameter lets apps pass arbitrary typed `Action` enums without forcing a generic on the trait (which would break object-safety); implementations downcast via `action.downcast_ref::<MyAction>()` to recover the concrete type.

### Implementing a custom provider

```rust
use djogi::auth::{AuthContext, AuthError, DjogiAuth};
use djogi::prelude::*;
use std::future::Future;
use std::pin::Pin;

// `raw_query` requires `T: FromPgRow` — tuples don't implement it, so
// define a small row struct with a hand-written decoder. `FromPgRow`
// is emitted by `#[model(...)]` for full models; for ad-hoc
// projections like this, implement it manually following the column
// order baked into the SELECT.
struct SessionRow {
 user_id: HeerId,
 tenant_id: Option<String>,
 scopes: Vec<String>,
}

impl djogi::FromPgRow for SessionRow {
 const COLUMNS: &'static [&'static str] = &["user_id", "tenant_id", "scopes"];
 const COLUMN_LIST: &'static str = "user_id, tenant_id, scopes";

 fn from_pg_row(row: &tokio_postgres::Row) -> Result<Self, djogi::DjogiError> {
 Ok(Self {
  user_id: row
 .try_get("user_id")
 .map_err(|e| djogi::DjogiError::Decode(format!("column `user_id`: {e}")))?,
  tenant_id: row
 .try_get("tenant_id")
 .map_err(|e| djogi::DjogiError::Decode(format!("column `tenant_id`: {e}")))?,
  scopes: row
 .try_get("scopes")
 .map_err(|e| djogi::DjogiError::Decode(format!("column `scopes`: {e}")))?,
 })
 }
}

pub struct MySessionProvider {
 pool: djogi::pg::pool::DjogiPool,
}

impl DjogiAuth for MySessionProvider {
 #[djogi::deliberately_bypass_convention_with_raw_sql]
 // JUSTIFICATION (djogi#234): provider-owned `sessions` table is not modelled
 // as a djogi `#[model]`; the typed surface does not cover lookup-by-token.
 fn authenticate<'a>(
 &'a self,
 token: &'a str,
 ) -> Pin<Box<dyn Future<Output = Result<AuthContext, AuthError>> + Send + 'a>> {
 Box::pin(async move {
  // Look the token up in your sessions table and build AuthContext.
  // `raw_query::<SessionRow>` returns Vec<SessionRow>; take the first
  // row via `.into_iter().next()` and map the empty case to
  // AuthError::InvalidToken.
  let mut ctx = djogi::DjogiContext::from_pool(self.pool.clone());
  let rows: Vec<SessionRow> = ctx
 .raw_query::<SessionRow>(
   "SELECT user_id, tenant_id, scopes FROM sessions \
   WHERE token = $1 AND expires_at > now()",
   &[&token],
  )
 .await
 .map_err(|e| AuthError::Provider(Box::new(e)))?;

  let SessionRow { user_id, tenant_id, scopes } = rows
 .into_iter()
 .next()
 .ok_or(AuthError::InvalidToken)?;
  let mut auth = AuthContext::new(user_id).with_scopes(scopes);
  if let Some(tid) = tenant_id {
  auth = auth.with_tenant(tid);
  }
  Ok(auth)
 })
 }

 fn verify<'a>(
 &'a self,
 ctx: &'a AuthContext,
 action: &'a dyn std::any::Any,
 ) -> Pin<Box<dyn Future<Output = Result<(), AuthError>> + Send + 'a>> {
 let _ = (ctx, action);
 Box::pin(async move {
  // Fail closed: deny by default. A real provider downcasts
  // `action` via `action.downcast_ref::<MyAction>()`, evaluates
  // an explicit policy against the resolved `AuthContext`, and
  // returns `Ok(())` only when that policy authorises the
  // caller for the requested action. Returning `Ok(())`
  // unconditionally is permitted but must be a deliberate,
  // code-reviewed choice — the trait deliberately has no
  // default body so the decision is visible at every impl site.
  Err(AuthError::Denied {
  reason: "no policy authorises this action".to_string(),
  })
 })
 }
}
```

The `#[djogi::deliberately_bypass_convention_with_raw_sql]` attribute is
mandatory wherever `ctx.raw_*` appears — it brings the sealed
`djogi::__bypass::RawAccessExt` trait into scope for the decorated item.
Pair it with an adjacent `// JUSTIFICATION (djogi#<n>):...` comment that
names the typed-surface gap the bypass is filling (see
[Raw SQL escape hatches](../spec/raw-sql-escape-hatches.md)). Modelling
the sessions table as a `#[model]` and using `Session::objects().filter(...)`
removes the need for the bypass entirely; the raw form is shown here only
because adopter-owned schemas frequently predate the model layer.

The `token` parameter is opaque to the trait — implementations decide whether to treat it as a session id, JWT, bearer, cookie value, or anything else. Djogi does not impose a format.

## `AuthContext`

```rust
#[derive(Debug, Clone)]
pub struct AuthContext {
 pub user_id: HeerId,
 pub tenant_id: Option<String>,
 pub scopes: Vec<String>,
 pub ext: std::collections::HashMap<String, String>,
}
```

Four fields cover the 95% case. `user_id` is `HeerId` in the shipped auth substrate; applications whose user model uses Djogi's recency-biased default PK should convert or normalize at the auth boundary until the auth context grows a generic/custom-PK story. `tenant_id` is `Option<String>` — strings rather than `HeerId` because tenant identity formats vary across deployments (UUIDs, slugs, external IDs). `scopes` carries OAuth-style permission strings. `ext` is a free-form string-to-string map for app-specific attributes without forcing trait objects or generics on the struct.

Builders:

```rust
let auth = AuthContext::new(user_id)
.with_tenant("org_42")
.with_scopes(vec!["read".into(), "write".into()]);

if auth.has_scope("admin") {
 //...
}
```

## Attaching auth to a context

Two shapes — consuming (before a transaction) or mutating (inside an `atomic()` closure):

```rust
use djogi::auth::AuthContext;

// Consuming builder — fine on a freshly-built ctx.
let ctx = djogi::DjogiContext::from_pool(pool.clone())
.with_auth(auth.clone());

// Mutating form — for use inside atomic() closures where the closure
// receives &mut DjogiContext.
djogi::transaction::atomic(&pool, |tx| Box::pin(async move {
 tx.set_auth(auth);
 //... CRUD / QuerySet ops here...
 Ok::<_, djogi::DjogiError>(())
})).await?;
```

Read the attached context back via `ctx.auth()`, which returns `Option<&AuthContext>`.

```rust
if let Some(auth) = ctx.auth() {
 println!("user = {}, tenant = {:?}", auth.user_id, auth.tenant_id);
}
```

## Auto-`set_tenant` integration

The big ergonomic win: once you've attached an `AuthContext` whose `tenant_id.is_some()`, **every CRUD and QuerySet operation on a tenant-keyed model automatically issues `set_tenant(auth.tenant_id)` before the query executes.** You never have to remember to call `ctx.set_tenant(...)` manually.

```rust
#[model(table = "posts", tenant_key = "org_id")]
pub struct Post {
 pub org_id: String,
 pub title: String,
}

djogi::transaction::atomic(&pool, |tx| Box::pin(async move {
 tx.set_auth(AuthContext::new(user_id).with_tenant("org_42"));
 // No explicit set_tenant call needed — the next query auto-issues it.
 let posts = Post::objects().fetch_all(tx).await?;
 Ok::<_, djogi::DjogiError>(posts)
})).await?;
```

Under the hood `DjogiContext` tracks the currently-applied tenant id and re-issues `SET LOCAL app.tenant_id =...` whenever the auth's `tenant_id` changes mid-transaction. That means switching tenants inside one `atomic()` scope is safe:

```rust
djogi::transaction::atomic(&pool, |tx| Box::pin(async move {
 tx.set_auth(auth_org_a);
 let a_posts = Post::objects().fetch_all(tx).await?; // scoped to org_a

 tx.set_auth(auth_org_b);
 let b_posts = Post::objects().fetch_all(tx).await?; // scoped to org_b
 // ^ auto-wiring notices applied_tenant_id != "org_b" and re-issues SET LOCAL.
 Ok::<_, djogi::DjogiError>((a_posts, b_posts))
})).await?;
```

For introspection / debugging you can read the currently-applied tid via `ctx.applied_tenant_id() -> Option<&str>`.

### When `tenant_id` is missing

If `ctx.auth()` is `Some` but `auth.tenant_id.is_none()` on a tenant-keyed model, Djogi emits a `tracing::warn!` on every CRUD / terminal call:

```
WARN djogi::query::terminal: auth attached but tenant_id is None on a tenant-keyed model;
queries will span tenants — call ctx.with_no_tenant_scope() to suppress
```

This closes a silent-cross-tenant-leak footgun — "forgot to thread tenant upstream" is now visible in logs instead of invisibly spanning tenants.

For deliberate cross-tenant flows (admin tooling, batch jobs, migrations), opt out explicitly:

```rust
let ctx = DjogiContext::from_pool(pool).with_no_tenant_scope();
// or, inside atomic():
tx.set_no_tenant_scope();
```

Without auth attached at all (`ctx.auth().is_none()`), no warn fires — that scenario is pre-auth context, not a forgotten tenant.

## `PasswordHash`

Behind the `auth-argon2` feature flag:

```toml
[dependencies]
djogi = { version = "...", features = ["auth-argon2"] }
```

```rust
use djogi::auth::PasswordHash;

// Hash on signup — uses Argon2id with the argon2 crate's default params.
let hash = PasswordHash::hash("s3cret").unwrap();

// Verify on login — constant-time; returns bool (never a Result), so
// error paths don't leak timing information through variants.
assert!(hash.verify("s3cret"));
assert!(!hash.verify("wrong"));
```

The stored column type is `TEXT`. The PHC string format (`$argon2id$...`) self-describes the algorithm, parameters, salt, and hash in one self-contained string — letting Djogi migrate between algorithms or param sets without schema changes.

`PasswordHash` implements `postgres_types::{ToSql, FromSql}` transparently, so you can drop it into any `#[model]` like any other field type:

```rust
use djogi::auth::PasswordHash;

#[model(table = "users")]
pub struct User {
 pub email: String,
 pub password_hash: PasswordHash,
}
```

Without the `auth-argon2` feature, `PasswordHash::hash` is unavailable but `PasswordHash::verify` still compiles (returns `false` — use the feature if you need actual verification).

## `AuthError`

```rust
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AuthError {
 #[error("invalid token")]
 InvalidToken,
 #[error("expired session")]
 ExpiredSession,
 #[error("missing auth context")]
 MissingAuth,
 #[error("authorization denied: {reason}")]
 Denied { reason: String },
 #[error("provider error: {0}")]
 Provider(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}
```

`AuthError` is `#[non_exhaustive]` — always include a wildcard arm when matching so future variants (rate-limit-exceeded, MFA-required, etc.) don't break downstream code. Database / driver failures do **not** route through `AuthError`; they surface as `DjogiError::Db(DbError)` from the substrate level. Provider-internal failures (JWT parse, HTTP fetch to an OIDC issuer, etc.) wrap via `AuthError::Provider(Box<dyn Error + Send + Sync>)`.

`AuthError` auto-converts into `DjogiError::Auth(AuthError)` for ergonomic `?`-propagation through `atomic()` / CRUD / QuerySet code paths.

## `_insecurely` bypass methods

Every `with_auth` / `set_auth` has an `_insecurely` sibling that emits a `tracing::warn!` identifying the call site:

```rust
// Emits: WARN auth guard bypassed via with_auth_insecurely caller=<file>:<line>
let ctx = ctx.with_auth_insecurely(auth);

// Same, mutating form:
ctx.set_auth_insecurely(auth);
```

These exist for **code with manually-established safety invariants** — tests, migrations, admin tooling, service-account flows. Calling `_insecurely` inside a production request handler is a design smell: if the situation genuinely requires bypassing an auth guard, name the pattern explicitly at the caller rather than silently skipping the check.

The same convention applies to 's tenant bypass (`_insecurely` CRUD methods on tenant-keyed models) — every bypass call site in the repo is grep-able via the `_insecurely` suffix and the `auth guard bypassed` / `tenant scope bypassed` log messages.

## Canonical users schema

A minimal schema that uses every primitive:

```sql
CREATE TABLE users (
 id  BIGINT PRIMARY KEY DEFAULT heerid_next_desc(),
 created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
 updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
 email  TEXT UNIQUE NOT NULL,
 password_hash TEXT NOT NULL
);

CREATE INDEX users_email_idx ON users (email);
```

```rust
use djogi::auth::PasswordHash;

#[model(table = "users")]
#[derive(Debug, Clone)]
pub struct User {
 pub email: String,
 pub password_hash: PasswordHash,
}
```

Signup and login become straight CRUD:

```rust
// Signup
let user = User::create(&mut ctx, User {
 email: "alice@example.com".to_string(),
 password_hash: PasswordHash::hash("s3cret").unwrap(),
..Default::default()
}).await?;

// Login
let user = User::objects()
.filter(|u| u.email().eq("alice@example.com"))
.fetch_one(&mut ctx)
.await?;
if !user.password_hash.verify(submitted_password) {
 return Err(AuthError::InvalidToken.into());
}
```

Session management (issuing tokens, storing server-side state, expiring rows) is application-layer — Djogi does not ship a `sessions` table, a cookie format, or a bearer-token scheme. Your `DjogiAuth` implementation decides.

## Sequencing

- Attach `auth` before or inside an `atomic()` scope.
- When the model is tenant-keyed, the first CRUD / terminal call inside that scope auto-issues `set_tenant(auth.tenant_id)`.
- The tenant id stays in effect for the rest of the transaction — or, if you `set_auth` again with a different tenant, the next query re-issues.
- Auth is scoped to the context; when the context drops (or the `atomic()` scope ends), the auth state goes with it.

## See also

- [Tenancy](./tenancy.md) — `#[model(tenant_key = "...")]`, RLS policies, `set_tenant`, `_insecurely` bypass.
- [Transactions](./transactions.md) — `DjogiContext`, `atomic()`, savepoints, on_commit callbacks.
- [Models](./models.md) — using `PasswordHash` or any custom typed field in `#[model]` structs.
