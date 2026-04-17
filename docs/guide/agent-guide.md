> [Back to README](../../ReadMe.MD) | [All Guides](./index.md)

# Agent Guide

This guide is written for AI coding agents (Claude, GPT, Cursor, etc.) working in a Djogi codebase. Read it at the start of a session before touching any model, query, or migration code.

Djogi is a Model-first ORM for Rust on Postgres. The key property: you never write SQL by hand. You define Rust structs, and Djogi generates ORM methods, migrations, and audit infrastructure. Your job as an agent is to work within that derivation chain — not around it.

---

## 1. Understanding a Model Definition

When you read a model, this is what you are looking at:

```rust
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[model(
    table = "invoices",
    tenant_key = "org_id",
    rationale = "One invoice per billing period per org. \
                 Do not update status directly — use invoice::transition()."
)]
pub struct Invoice {
    pub org_id: HeerId,
    pub number: String,
    pub total_cents: i64,
    pub status: String,
    #[field(rationale = "Set once on creation, never updated. Null means perpetual license.")]
    pub due_date: Option<time::Date>,
    #[field(lazy)]
    pub line_items_json: Lazy<String>,
}
```

**What this tells you:**

- `table = "invoices"` — the Postgres table name is `invoices`
- `tenant_key = "org_id"` — this is a multi-tenant model. **You must call `djogi::set_tenant()` before every query.** Failing to do so will result in a runtime error in production mode or incorrect data access in development.
- `rationale = "..."` — the model has behavioral constraints that are not in the type system. Read the rationale before writing any code that modifies invoices.
- `org_id: HeerId` — tenant discriminator, must be set on every `create()` call
- `number: String` — unbounded text
- `total_cents: i64` — monetary amount in cents (integers, not floats)
- `status: String` — unvalidated string. Check the rationale for valid values.
- `due_date: Option<time::Date>` — nullable DATE column. The `#[field(rationale)]` tells you it is write-once.
- `#[field(lazy)] line_items_json: Lazy<String>` — not loaded in `fetch_all()`. Call `.load(&pool).await?` to retrieve it.

**What is injected by the macro (not written in the struct):**

- `id: HeerId` — BIGINT PRIMARY KEY, populated via `RETURNING` after INSERT
- `created_at: time::OffsetDateTime` — TIMESTAMPTZ, set by the DB on INSERT
- `updated_at: time::OffsetDateTime` — TIMESTAMPTZ, updated by Djogi on every `save()`

**What methods are available:**

After `#[derive(Model)]`, the following are always available:

| Method | Signature | Notes |
|---|---|---|
| `Invoice::objects()` | `-> QuerySet<Invoice>` | Start a lazy query |
| `Invoice::get(pool, id)` | `async -> Result<Invoice>` | Fetch by PK, returns `Err(NotFound)` if missing |
| `Invoice::create(pool, value)` | `async -> Result<Invoice>` | INSERT + RETURNING |
| `invoice.save(pool)` | `async -> Result<()>` | UPDATE (full row or dirty fields) |
| `invoice.delete(pool)` | `async -> Result<()>` | DELETE, consumes instance |
| `Invoice::descriptor()` | `-> ModelDescriptor` | For registration — do not call manually |

For tenant-keyed models, the following are also available:

| Method | Notes |
|---|---|
| `Invoice::objects().fetch_all_insecurely(&pool)` | Bypasses RLS — log-generating, admin use only |

---

## 2. The Golden Rules

Follow these unconditionally. Every violation is either a bug or a security vulnerability.

### Rule 1: Never SELECT *

The `QuerySet` API selects exactly the columns that are defined on the struct (minus `Lazy<T>` fields). You do not write `SELECT *`. If you are writing raw SQL via `sqlx::QueryBuilder`, list columns explicitly. Never use `SELECT *` in any query.

### Rule 2: Always call `set_tenant()` for tenant-keyed models

If the model has `#[model(tenant_key = "...")]`, you must:

1. Begin a transaction: `let mut tx = pool.begin().await?`
2. Set the tenant: `djogi::set_tenant(&mut tx, tenant_id).await?`
3. Pass `&mut tx` (not `&pool`) to all query methods in that scope
4. Commit or roll back: `tx.commit().await?`

There is no exception. Even if you are "just reading" — the RLS policy uses the session variable, which is transaction-scoped.

```rust
// CORRECT
let mut tx = pool.begin().await?;
djogi::set_tenant(&mut tx, org_id).await?;
let invoices = Invoice::objects()
    .filter(|f| f.status.eq("open"))
    .fetch_all(&mut tx).await?;
tx.commit().await?;
```

### Rule 3: Use `_insecurely()` only with explicit rationale

If you find yourself reaching for `fetch_all_insecurely()`, `fetch_one_insecurely()`, or `query_insecurely()`, stop and ask:

- Is this in a request handler path? If yes, do not use `_insecurely()`. Find the correct tenant-scoped approach.
- Is this in admin tooling, a migration script, or a background job that genuinely needs cross-tenant access? If yes, leave a comment explaining why.

All `_insecurely()` calls generate audit log entries. If this is reviewed later, the reasoning should be clear.

### Rule 4: Read `rationale` before touching a model

If a model or field has a `#[model(rationale = "...")]` or `#[field(rationale = "...")]`, read it before writing any code. The rationale captures behavioral constraints, write patterns, ownership rules, and retention policies that the type system cannot encode. Ignoring it produces bugs that are invisible until production.

To see all rationale strings for current models, run:

```bash
cargo djogi docs
```

### Rule 5: Never write migration files by hand

Djogi generates migration files from model definitions during `cargo build`. You modify the struct — the build script detects drift and generates the SQL. You review the SQL, then run `cargo djogi migrate`. You do not write `ALTER TABLE` statements by hand.

---

## 3. How to Write a New Model — Step by Step

**Step 1: Identify the table name and primary key type.**

Default PK is `HeerId` (64-bit BIGINT). Use `pk = "ranjid"` for high-volume event tables. Use `pk = "serial"` only for small reference tables (country codes, status types).

**Step 2: Write the struct with developer-owned fields only.**

Do not write `id`, `created_at`, or `updated_at` — the macro injects these.

```rust
// src/apps/billing/models.rs
use djogi::prelude::*;

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[model(table = "subscriptions")]
pub struct Subscription {
    pub customer_id: ForeignKey<Customer>,
    pub plan_id: ForeignKey<Plan>,
    pub status: String,
    pub current_period_end: time::OffsetDateTime,
    pub cancel_at_period_end: bool,
}
```

**Step 3: Register the model in the app.**

```rust
// src/apps/billing/mod.rs
use djogi::prelude::*;

struct BillingApp;

impl App for BillingApp {
    fn models() -> &'static [ModelDescriptor] {
        &[Subscription::descriptor(), Plan::descriptor()]
    }
    fn routes() -> axum::Router {
        routes::billing_router()
    }
}

djogi::register_app!(BillingApp);
```

**Step 4: Build and review the generated migration.**

```bash
cargo build
```

You will see a compiler diagnostic pointing to the generated migration files. Read the SQL before proceeding:

```bash
cat migrations/0004_create_subscriptions_up.sql
```

**Step 5: Apply the migration.**

```bash
cargo djogi migrate
```

**Step 6: Write your business logic using the generated methods.**

```rust
let sub = Subscription::create(&pool, Subscription {
    customer_id: customer.id.into(),
    plan_id: plan.id.into(),
    status: "active".into(),
    current_period_end: next_month,
    cancel_at_period_end: false,
    ..Default::default()
}).await?;
```

---

## 4. How to Add a New Field Safely

**The risk: treating a rename as a DROP + ADD.**

If you rename a field without annotating it, the differ generates:

```sql
ALTER TABLE subscriptions DROP COLUMN notes;     -- data destroyed
ALTER TABLE subscriptions ADD COLUMN description TEXT NOT NULL DEFAULT '';
```

**The correct process for a new field:**

Simply add the field to the struct. The build script detects the new column and generates `ADD COLUMN` SQL. Build, review, migrate.

```rust
pub struct Subscription {
    pub customer_id: ForeignKey<Customer>,
    pub plan_id: ForeignKey<Plan>,
    pub status: String,
    pub current_period_end: time::OffsetDateTime,
    pub cancel_at_period_end: bool,
    pub notes: Option<String>,   // new nullable field — generates ADD COLUMN notes TEXT
}
```

**The correct process for a rename:**

Annotate with `renamed_from` before building. This tells the differ to generate `RENAME COLUMN` instead of `DROP + ADD`.

```rust
#[field(renamed_from = "notes")]   // was: pub notes: Option<String>
pub description: Option<String>,
```

Generated SQL:
```sql
ALTER TABLE subscriptions RENAME COLUMN notes TO description;
```

Remove the `renamed_from` annotation after the migration has been applied to all environments (local, staging, production). Leaving it in longer than necessary is harmless but confusing.

**The correct process for a field deletion:**

Deleting a field from the struct generates `DROP COLUMN`. The differ will warn and refuse unless `--allow-destructive` is passed to `makemigrations`. Before deleting:

1. Check that the column is not referenced by any query, index, or foreign key
2. Deploy application code that stops reading/writing the column
3. After confirming no queries reference it, delete the struct field
4. Generate the migration: `cargo djogi makemigrations --allow-destructive`
5. Review the down migration — it will regenerate the column with a default value, but any data that was in the column is unrecoverable after `rollback`

---

## 5. How to Query Safely

### Avoid full-table scans

Every filter should use an indexed column where possible. Check what indexes exist:

```bash
cargo djogi analyze --table subscriptions
```

If you are filtering on a column that is not indexed and the table has more than a few thousand rows, add `#[field(index)]` to the field definition and let the migration system add the index.

### Partition-key requirements

If the model has `#[model(partition_by = "...")]`, queries that do not constrain the partition key result in full partition scans. Djogi emits a compile-time warning when it detects this pattern.

```rust
// Model: #[model(table = "events", partition_by = "range:occurred_at")]

// CORRECT — constrains the partition key
Event::objects()
    .filter(|f| f.occurred_at.gte(start).and(f.occurred_at.lt(end)))
    .fetch_all(&pool).await?;

// WRONG — full partition scan, expect a compiler warning
Event::objects()
    .filter(|f| f.kind.eq("user.signup"))
    .fetch_all(&pool).await?;
```

### Prefetch instead of fetching in a loop

When you need related records, use `.prefetch()` on the `QuerySet`. Never fetch related records in a loop.

```rust
// WRONG — N+1 queries
let subs = Subscription::objects().fetch_all(&pool).await?;
for sub in &subs {
    let customer = sub.customer_id.fetch(&pool).await?;  // one query per sub
    println!("{}: {}", customer.name, sub.status);
}

// CORRECT — 2 queries total regardless of how many subs
let subs = Subscription::objects()
    .prefetch(SubscriptionRelated::customer())
    .fetch_all(&pool).await?;
for sub in &subs {
    let customer = sub.customer_id.resolved();  // free — already loaded
    println!("{}: {}", customer.map(|c| c.name.as_str()).unwrap_or("?"), sub.status);
}
```

---

## 6. How to Use the Transactional Outbox Correctly

The transactional outbox guarantees that events are published if and only if the database write succeeds. It requires a model annotated with `#[model(events)]`.

```rust
#[model(table = "orders", events)]
pub struct Order { ... }
```

**Correct pattern — all in one transaction:**

```rust
let mut tx = pool.begin().await?;

// For tenant-keyed outbox models:
djogi::set_tenant(&mut tx, org_id).await?;

let order = Order::create_in_tx(&mut tx, Order {
    customer_id: customer.id.into(),
    total_cents: 9900,
    status: "pending".into(),
    ..Default::default()
}).await?;

// Publish event atomically — event is written to _djogi_outbox in the same transaction
order.publish_event(&mut tx, "order.created", &order).await?;

// Both the INSERT and the outbox row commit together
tx.commit().await?;

// The outbox worker picks up the event and delivers it externally
// If commit() fails, neither the order nor the event is published
```

**Common mistake — creating outside a transaction:**

```rust
// WRONG — event is not atomic with the INSERT
let order = Order::create(&pool, ...).await?;
order.publish_event(&pool, "order.created", &order).await?;  // what if this fails?
```

If `publish_event` fails after `create` succeeds, the order exists but the event is never published. Always use `create_in_tx` and pass the transaction to both calls.

---

## 7. How `#[field(rationale)]` Guides Your Decisions

`#[field(rationale = "...")]` is a machine-readable advisory attached to a specific field. It communicates:

- **Write restrictions:** "Set once on creation, never updated"
- **Value semantics:** "Null means the user has never purchased"
- **Ownership rules:** "Updated only by the billing cron job, never by user input"
- **Data quality notes:** "Migrated from a legacy system; values before 2022 may be approximate"

When you encounter a `rationale`, treat it as a constraint on your implementation. If you are writing code that violates what the rationale says — e.g., updating a field the rationale says is write-once — stop and verify your understanding with the developer before proceeding.

Run `cargo djogi docs` to see all rationale strings in rendered Markdown for the current codebase.

---

## 8. Using `cargo djogi docs` to Understand the Current Model State

Before modifying existing models or writing new queries, get a current picture of the codebase's model landscape:

```bash
cargo djogi docs
```

This writes `docs/models/*.md` — one file per model — with:
- Table name, PK type, RLS configuration
- All fields with types, nullability, indexes, and rationale strings
- `#[model(...)]` attributes (tenant_key, partition_by, idempotency_key, etc.)
- M2M relationship graph (which models are connected and through what)

Then verify that existing docs are consistent with the current definitions:

```bash
cargo djogi check-docs
```

If `check-docs` reports drift, re-run `cargo djogi docs` and review the changes before writing code that depends on the old documentation.

---

## 9. Common Mistakes and How to Avoid Them

### Mistake: Forgetting `..Default::default()` on `create()`

The macro injects `id`, `created_at`, and `updated_at` as real struct fields. When constructing a value to pass to `create()`, you must provide every field or use struct update syntax. The easiest approach is `..Default::default()`:

```rust
// WRONG — will not compile (missing id, created_at, updated_at)
let sub = Subscription::create(&pool, Subscription {
    customer_id: customer.id.into(),
    plan_id: plan.id.into(),
    status: "active".into(),
    current_period_end: next_month,
    cancel_at_period_end: false,
}).await?;

// CORRECT
let sub = Subscription::create(&pool, Subscription {
    customer_id: customer.id.into(),
    plan_id: plan.id.into(),
    status: "active".into(),
    current_period_end: next_month,
    cancel_at_period_end: false,
    ..Default::default()   // fills id, created_at, updated_at with zero values
}).await?;                 // framework replaces them before INSERT
```

### Mistake: Accessing `Lazy<T>` fields without loading them

```rust
let articles = Article::objects().fetch_all(&pool).await?;

// WRONG — body is a Lazy<String>, not loaded in fetch_all()
println!("{}", articles[0].body);  // prints empty string, not the actual body
```

```rust
// CORRECT — explicitly load the lazy field
let body: String = articles[0].body.load(&pool).await?;
println!("{}", body);
```

### Mistake: Using `_insecurely()` to work around `set_tenant()`

```rust
// WRONG — bypasses isolation because it seems easier than setting up a transaction
Invoice::objects()
    .filter(|f| f.org_id.eq(org_id))
    .fetch_all_insecurely(&pool).await?;
```

The filter is application-layer only — a bug in the `org_id` extraction exposes all tenants. Use a transaction with `set_tenant()`:

```rust
// CORRECT
let mut tx = pool.begin().await?;
djogi::set_tenant(&mut tx, org_id).await?;
Invoice::objects()
    .filter(|f| f.status.eq("open"))
    .fetch_all(&mut tx).await?;
tx.commit().await?;
```

### Mistake: Writing a migration file by hand

Do not create files in `migrations/` manually. Djogi's differ owns migration generation. If you create a hand-written migration, the differ will not know about it and may generate conflicting migrations on the next build.

If you need to apply custom SQL (data migrations, materialized views, functions), write it as a separate SQL file in `migrations/custom/` and reference it from a migration script. Discuss the pattern with the developer before implementing it.

### Mistake: Renaming a field without `renamed_from`

```rust
// WRONG — generates DROP + ADD (data destruction)
pub struct Vehicle {
    pub make: String,
    pub model_name: String,  // was: pub model: String
}
```

```rust
// CORRECT — tells the differ to generate RENAME COLUMN
pub struct Vehicle {
    pub make: String,
    #[field(renamed_from = "model")]
    pub model_name: String,
}
```

### Mistake: Using `chrono` instead of `time`

Djogi uses the `time` crate for all datetime types. Do not add `chrono` as a dependency or use `chrono::DateTime` in model fields. Use `time::OffsetDateTime` for timestamps and `time::Date` for dates.

### Mistake: Adding `serde_json::Value` where a schema exists

If the JSON structure is known, use `Jsonb<T>` instead of `serde_json::Value`. `Jsonb<T>` gives you:
- Compile-time typed access to known fields
- Validation before save
- Unknown field preservation (not silent data loss)
- Typed filter closures for subfield queries

Only use `serde_json::Value` when the structure is genuinely dynamic and cannot be modeled.

### Mistake: Calling `.save()` on a record fetched from a different pool than the one you write to

In multi-database configurations (app + CRUD log + event log), the pools are distinct. Model instances fetched from one pool should be saved to the same pool. If you use a transaction, pass the transaction consistently through the entire read-write cycle.

---

## Quick Reference

| Task | Correct approach |
|---|---|
| Create a record | `Model::create(&pool, Model { ..., ..Default::default() }).await?` |
| Fetch by PK | `Model::get(&pool, id).await?` |
| Update a field | `instance.field = value; instance.save(&pool).await?` |
| Delete | `instance.delete(&pool).await?` |
| Filter query | `Model::objects().filter(\|f\| f.field.eq(val)).fetch_all(&pool).await?` |
| Tenant-scoped query | `begin tx → set_tenant() → query with &mut tx → commit` |
| Load a lazy field | `instance.field.load(&pool).await?` |
| Prefetch relations | `.prefetch(ModelRelated::relation()).fetch_all(&pool).await?` |
| Cross-tenant admin query | `Model::objects().fetch_all_insecurely(&pool).await?` with comment |
| Add a field | Add to struct → `cargo build` → review SQL → `cargo djogi migrate` |
| Rename a field | `#[field(renamed_from = "old")]` → build → review → migrate → remove annotation |
| Delete a field | Remove from struct → `cargo djogi makemigrations --allow-destructive` → review → migrate |
| See model rationale | `cargo djogi docs` |
| Verify doc currency | `cargo djogi check-docs` |
