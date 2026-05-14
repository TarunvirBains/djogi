> [Back to roadmap index](./index.md) | [Shipped model guide](../guide/models.md)

# Models (roadmap)

> **Status: SHIPPED.** Phases 3 through 7.5 delivered the model surface
> this document was designed against — relations (`ForeignKey<T>`,
> `OneToOneField<T>`, explicit-through M2M), eager loading, typed JSONB,
> typed enums, arrays, GeoPoint + spatial geometries, full-text search,
> RLS via `tenant_key`, partitioned tables (`partition_by`), and
> protected-data attrs. The authoritative current API lives in
> [`docs/guide/models.md`](../guide/models.md) and the
> feature-specific guides ([Relations](../guide/relations.md),
> [JSONB](../guide/jsonb.md), [Spatial](../guide/spatial.md), etc.).
> This roadmap document is preserved as design history.

This document is the design reference for model-level attributes that the
`#[model]` macro does not yet support. Each section states the phase that
will deliver it.

---

## Model Attributes (aspirational)

### `partition_by = "range:column"` | `"hash:column:N"` — Phase 7

Declares the table as a Postgres partitioned table. Djogi generates the `PARTITION BY` clause in the `CREATE TABLE` migration and includes partition-key requirements in query warnings.

**Range partitioning** — used for time-series or date-bucketed data:

```rust
#[model(table = "events", partition_by = "range:occurred_at")]
#[derive(Debug, Clone)]
pub struct Event {
    pub occurred_at: time::OffsetDateTime,
    pub kind: String,
    pub payload: serde_json::Value,
}
```

Generated SQL:
```sql
CREATE TABLE events (
    id           BIGINT PRIMARY KEY DEFAULT heerid_next_desc(),
    occurred_at  TIMESTAMPTZ NOT NULL,
    kind         TEXT NOT NULL,
    payload      JSONB NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
) PARTITION BY RANGE (occurred_at);
```

**Hash partitioning** — distributes rows evenly across N partitions by hash of the partition key:

```rust
#[model(table = "user_events", partition_by = "hash:user_id:8")]
#[derive(Debug, Clone)]
pub struct UserEvent {
    pub user_id: i64,    // ForeignKey<User> is Phase 3
    pub kind: String,
}
```

Generated SQL:
```sql
CREATE TABLE user_events (
    id       BIGINT PRIMARY KEY DEFAULT heerid_next_desc(),
    user_id  BIGINT NOT NULL,
    kind     TEXT NOT NULL,
    ...
) PARTITION BY HASH (user_id);
```

> Queries against partitioned tables that do not include the partition key in the `WHERE` clause result in full partition scans. Djogi emits a compile-time warning when it detects a `QuerySet` filter on a partitioned table that does not constrain the partition key.

---

### `idempotency_key = "field_name"` — Phase 3 (create_or_find)

Designates a field as an idempotency key, enabling the `create_or_find()` method. The named field must be a unique column (Djogi adds the `UNIQUE` constraint automatically).

```rust
#[model(table = "payments", idempotency_key = "external_ref")]
#[derive(Debug, Clone)]
pub struct Payment {
    pub external_ref: String,   // UNIQUE — used as idempotency key
    pub amount_cents: i64,
    pub currency: String,
    pub status: String,
}
```

This generates:

```rust
// Returns the existing record if external_ref already exists, or creates a new one.
// Equivalent to: INSERT ... ON CONFLICT (external_ref) DO NOTHING RETURNING *
// followed by SELECT ... WHERE external_ref = $1 if nothing was inserted.
let mut ctx = DjogiContext::from_pool(pool.clone());
Payment::create_or_find(&mut ctx, Payment {
    external_ref: "pay_ABC123".into(),
    amount_cents: 9900,
    currency: "USD".into(),
    status: "pending".into(),
    // framework fields (id/created_at/updated_at) are ignored by create_or_find
    ..Default::default()
}).await?;
```

Use `create_or_find()` for webhook receivers, payment processors, or any external system that may deliver the same event more than once.

---

### `events` — Phase 5 (outbox)

Enables the transactional outbox pattern for this model. When set, every `create()`, `save()`, and `delete()` call can publish events atomically within the same database transaction.

```rust
#[model(table = "orders", events)]
#[derive(Debug, Clone)]
pub struct Order {
    pub customer_id: i64,    // ForeignKey<Customer> is Phase 3
    pub total_cents: i64,
    pub status: String,
}
```

With `events` enabled, the model gains:

```rust
// Create the record and publish an event in the same transaction
let mut ctx = DjogiContext::from_pool(pool.clone());
let mut tx_ctx = ctx.begin().await?;
let order = Order::create(&mut tx_ctx, Order {
    customer_id: customer_id,
    total_cents: 4999,
    status: "pending".into(),
    ..Default::default()
}).await?;
order.publish_event(&mut tx_ctx, "order.created", &order).await?;
tx_ctx.commit().await?;
// Both the INSERT and the outbox row commit together — or neither does.
```

The outbox table (`_djogi_outbox`) is created automatically. A background worker (provided by `djogi::outbox::Worker`) polls this table and delivers events to the configured sink (Kafka, NATS, HTTP webhook, etc.).

---

### `tenant_key = "field_name"` — Phase 5 (RLS)

Enables Row Level Security (RLS) isolation for multi-tenant models. The named field becomes the tenant discriminator, and Djogi generates the RLS policy SQL.

```rust
#[model(table = "invoices", tenant_key = "org_id")]
#[derive(Debug, Clone)]
pub struct Invoice {
    pub org_id: HeerId,       // must be present as an explicit field
    pub number: String,
    pub total_cents: i64,
    pub status: String,
}
```

When Phase 5 ships, `tenant_key` will integrate with `djogi::set_tenant()`, `TenantScoped<T>`, and `_insecurely()` methods for complete RLS coverage.

---

### `rationale = "..."` — Phase 5 (warnings and tooling)

An advisory documentation string. Does not affect generated code. Will be surfaced by `djogi docs` and in the admin panel — provides context for AI coding agents, new team members, and schema reviewers.

```rust
#[model(
    table = "audit_entries",
    rationale = "Append-only audit log. Records must never be updated or deleted. \
                 Retention policy: 7 years for compliance. Write via audit::record() only."
)]
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub actor_id: HeerId,
    pub action: String,
    pub target_table: String,
    pub target_id: HeerId,
    pub payload: serde_json::Value,
}
```

> **Note for AI coding agents (future):** When a model has a `rationale`, read it before writing any code that touches the model. The rationale captures constraints and intentions that are not expressible in the type system.

---

### `cache_ttl = N` — Phase 8 (Redis)

Opt-in Redis cache TTL in seconds. When set, `get()` and `fetch_one()` results are cached and invalidated on `save()` or `delete()`. Requires the `cache` feature flag and a Redis connection configured in `Djogi.toml`.

```rust
#[model(table = "country_codes", pk = Serial, cache_ttl = 3600)]
#[derive(Debug, Clone)]
pub struct CountryCode {
    pub iso_alpha2: String,
    pub name: String,
}
```

Use `cache_ttl` only for data that changes rarely and is read frequently — small reference tables, configuration rows, and similar. Never use it for user-mutable application data where stale reads are harmful.

---

### `dirty_tracking` — Phase 7

Enables per-field dirty tracking for this model (per-model override of the global `dirty_tracking` setting in `Djogi.toml`). When enabled, `save()` issues `UPDATE` only for fields that changed since the record was fetched.

```rust
#[model(table = "user_profiles", dirty_tracking)]
#[derive(Debug, Clone)]
pub struct UserProfile {
    pub display_name: String,
    pub bio: Option<String>,
    pub avatar_url: Option<String>,
}
```

```rust
let mut ctx = DjogiContext::from_pool(pool.clone());
let mut profile = UserProfile::get(&mut ctx, id).await?;
profile.bio = Some("Updated bio".into());
// Emits: UPDATE user_profiles SET bio = $1, updated_at = $2 WHERE id = $3
// display_name and avatar_url are NOT included in the UPDATE.
profile.save(&mut ctx).await?;
```

Without dirty tracking, `save()` always issues a full-row UPDATE for all fields.

---

## Field Attributes (aspirational)

### `#[field(lazy)]` — Phase 5

Marks the field as lazy-loaded. Lazy fields are excluded from `SELECT *` and from the default `FromRow` deserialization path. They are loaded only when explicitly requested via `.load()`.

```rust
#[model(table = "posts")]
#[derive(Debug, Clone)]
pub struct Post {
    pub title: String,
    pub slug: String,
    #[field(lazy)]
    pub body: String,         // TEXT column — not loaded in list queries
}
```

```rust
// body is not populated here
let mut ctx = DjogiContext::from_pool(pool.clone());
let posts = Post::objects().fetch_all(&mut ctx).await?;

// Load body for a specific post — one extra query
let body = posts[0].body.load(&mut ctx).await?;
```

Use `#[field(lazy)]` for large text or binary columns that are expensive to transfer and not needed in list views.

---

### `#[field(outbox = "ignore")]` — Phase 5 (outbox)

Excludes a field from the transactional outbox event payload. Use for secrets, internal tokens, or data that must not leave the database perimeter.

```rust
#[model(table = "users", events)]
#[derive(Debug, Clone)]
pub struct User {
    pub email: String,
    #[field(outbox = "ignore")]
    pub password_hash: String,  // excluded from outbox events — never emitted externally
    pub display_name: String,
}
```

---

### `#[field(rationale = "...")]` — Phase 5 (tooling)

An advisory documentation string on a field. Will be surfaced by `djogi docs` and in the admin panel.

```rust
#[field(rationale = "Stripe customer ID — set once on first payment, never updated. \
                     Null means the user has never purchased.")]
pub stripe_customer_id: Option<String>,
```

---

### `#[field(shadow_of = "old_column")]` — Phase 6 (zero-downtime migrations)

Dual-write support during online schema migrations. When a field is annotated with `shadow_of`, every `save()` writes the new field value and also writes the same value to the old column. This allows a zero-downtime rename: deploy the new field, backfill, verify, then remove the old column in a follow-up migration.

```rust
#[model(table = "users")]
#[derive(Debug, Clone)]
pub struct User {
    pub username: String,           // old column — being phased out
    #[field(shadow_of = "username")]
    pub handle: String,             // new column — writes to both handle and username
}
```

Remove `shadow_of` once the old column is confirmed unused and before dropping it.

---

## Field Types (aspirational)

### `ForeignKey<T>` — Phase 3

`ForeignKey<T>` stores the `id` of a related model as a `BIGINT` column with a foreign key constraint. The related record is not loaded automatically — you must call `.fetch()` or use `.prefetch()` on the `QuerySet`.

```rust
#[model(table = "comments")]
#[derive(Debug, Clone)]
pub struct Comment {
    #[field(on_delete = "cascade")]
    pub post_id: ForeignKey<Post>,     // BIGINT REFERENCES posts(id) ON DELETE CASCADE
    pub author_id: ForeignKey<User>,   // BIGINT REFERENCES users(id) ON DELETE RESTRICT
    pub body: String,
}

let mut ctx = DjogiContext::from_pool(pool.clone());

// Single fetch — one additional query
let post = comment.post_id.fetch(&mut ctx).await?;

// Prefetch on QuerySet — one IN(...) query per relation, not N+1
let comments = Comment::objects()
    .prefetch(CommentRelated::post())
    .prefetch(CommentRelated::author())
    .fetch_all(&mut ctx).await?;

// After prefetch, resolved() is free — no additional query
let post = comments[0].post_id.resolved();  // -> Option<&Post>
```

For a `ForeignKey<T>` field named `post_id`, the generated column is `post_id BIGINT NOT NULL REFERENCES posts(id)`. The suffix `_id` is convention — the column stores only the referenced ID.

In Phase 1, use a plain `HeerId` or `i64` field to store a foreign ID manually. Phase 3 replaces it with `ForeignKey<T>` and wires the constraint + prefetch machinery.

---

### `Jsonb<T>` — Phase 5

`Jsonb<T>` is a JSONB column with a typed Rust schema. The field validates its data before any write. Unknown fields (present in the database but absent from the schema) are preserved across every `save()`.

```rust
use djogi::prelude::*;

#[derive(JsonSchema, Serialize, Deserialize, Validate)]
pub struct EngineSpec {
    pub cylinders: i32,
    #[validate(range(min = 0, max = 2000))]
    pub horsepower: i32,
    pub turbo: Option<bool>,
}

#[model(table = "vehicles")]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub make: String,
    pub engine: Jsonb<EngineSpec>,   // JSONB NOT NULL
}
```

Internal layout:
```rust
pub struct Jsonb<T> {
    pub data: T,                           // typed, validated on save
    extra: IndexMap<String, UnknownField>, // unknown fields — never dropped
}
```

In Phase 1, use `serde_json::Value` for untyped JSONB. Phase 5 introduces `Jsonb<T>` with schema validation and unknown-field preservation.

---

### `GeoPoint` — Phase 5 (PostGIS)

A geographic point type backed by the PostGIS `GEOGRAPHY(Point, 4326)` column type. Requires the `postgis` feature and a Postgres installation with PostGIS enabled.

```rust
use djogi::types::GeoPoint;

#[model(table = "locations")]
#[derive(Debug, Clone)]
pub struct Location {
    pub name: String,
    pub coordinates: GeoPoint,   // GEOGRAPHY(Point, 4326) NOT NULL
}
```

Phase 5 will introduce spatial query operators (`within_radius`, `nearest_n`, etc.) as `QuerySet` extensions.

---

## Many-to-Many Relationships — Phase 3

Implicit M2M fields are not provided by Djogi. All M2M relationships require an explicit through model. This avoids the forced migration that implicit join tables eventually require when you need to store data on the relationship.

```rust
#[model(table = "person_groups")]
#[derive(Debug, Clone)]
pub struct PersonGroup {
    pub person_id: ForeignKey<Person>,
    pub group_id: ForeignKey<Group>,
    pub joined_at: time::OffsetDateTime,
    pub role: String,
}

// Declare the relationship in both directions — method name comes from RELATION, not auto-pluralized
impl ManyToMany<Group> for Person {
    type Through = PersonGroup;
    const RELATION: &'static str = "groups";   // generates person.groups()
}

impl ManyToMany<Person> for Group {
    type Through = PersonGroup;
    const RELATION: &'static str = "members";  // generates group.members()
}
```

Generated convenience methods (Phase 3):

```rust
let mut ctx = DjogiContext::from_pool(pool.clone());

// Person side
let groups = person.groups(&mut ctx).await?;
person.add_to_group(&mut ctx, &group, PersonGroup {
    role: "admin".into(),
    ..Default::default()
}).await?;
person.remove_from_group(&mut ctx, &group).await?;

// Group side
let members = group.members(&mut ctx).await?;

// Through model is a full Model — directly queryable
let admins = PersonGroup::objects()
    .filter(|f| f.role.eq("admin"))
    .fetch_all(&mut ctx).await?;
```
