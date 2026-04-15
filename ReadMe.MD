# Django Architecture & The Agentic Shift

## 1. Beyond File Separation: The True Goals of Django

While MVT (Model-View-Template) is the organizational map, Django's core "engine" is designed to solve systemic development problems:

- **DRY (Don't Repeat Yourself):** Define your data schema exactly once in `models.py`. Django then automatically derives the database migrations, the Python ORM API, and the Admin UI.
- **Loose Coupling:** The "Holy Grail" of its architecture. You should be able to swap your database (Model) or your frontend (Template) without rewriting the core business logic.
- **Security by Default:** It treats security as a framework responsibility, providing built-in protection against SQL injection, XSS, and CSRF that works "behind the scenes."
- **Batteries Included:** Providing a production-ready Admin site, Auth system, and Session management out of the box to eliminate "integration fatigue."

## 2. MVT vs. The Reality of the Request Cycle

MVT is a simplification. A real request hits several "hidden" layers that handle the heavy lifting:

| Component | The "Simplified" View | The Technical Reality |
|---|---|---|
| URL Router | Matches a URL to a file. | A complex regex/path engine that acts as the entry point for the Controller logic. |
| Middleware | Not mentioned in MVT. | The "Secret Service." It inspects/modifies every request and response (e.g., checking if a user is logged in before the View even sees them). |
| Managers/ORM | "The Database." | A sophisticated abstraction layer that translates Pythonic code into optimized SQL. |
| Forms/Serializers | "Part of the View." | The "Bouncer." A dedicated validation layer that ensures data integrity before it ever touches the Model. |

## 3. The Paradigm Shift: Then vs. Now

The value of a "Batteries Included" framework has been redefined by agentic coding (AI-driven development). Django remains productive — but the reason to reach for a full-stack framework has shifted.

| Feature | The Django Era (2005–2020) | The Agentic Era (2024+) |
|---|---|---|
| Main Hurdle | Integration: Connecting libraries was hard and slow. | Context: Managing complex, hidden abstractions is the new bottleneck. |
| Developer Speed | Provided by pre-written, high-level functions. | Provided by LLM generation and instant boilerplate creation. |
| Code Style | Magic & Abstraction: Hiding logic to save typing. | Explicit & Flat: AI performs better when logic is visible and type-safe. |
| Safety | Framework conventions (The "Django Way"). | Type systems (Rust/C#), AI-assisted security analysis, automated vulnerability detection, and model-driven fuzzing — the safety story now extends well beyond convention compliance. |
| Framework Role | A rigid foundation you build inside of. | A narrow, deep framework that owns the Model derivation chain (ORM, migrations, admin, audit) while delegating commoditized layers (routing, rendering) to the ecosystem and to AI. |

**Summary:** Django was liberating because it did the "boring stuff" for you. In an agentic workflow, the boring stuff — boilerplate, integration, routing, rendering — is now free and instant. What remains expensive is the data layer: schema consistency, migration safety, audit trails, type-safe queries across JSONB nesting depths. This is where framework-level ownership still pays for itself — and where implicit magic becomes a liability.

The preference has shifted not away from frameworks entirely, but away from full-stack MVT monoliths toward Model-first frameworks that are explicit, type-safe, and narrow in scope but deep within that scope. Djogi exists in this space: it takes Django's ideas (define the model, derive everything else) but rejects Django's implementation strategy (hiding logic to save typing) in favor of Rust's explicitness.


---

# Djogi Framework Specification

*A Model-first web framework for Rust, built on Axum*

---

## 1. Vision

Djogi is a Model-first framework: define your data schema as Rust structs, and the framework derives everything else — ORM, migrations, admin UI, audit trail, shell bindings, JSONB schema handling. One definition, full derivation chain.

Djogi does not own routing, middleware, or rendering. The View layer belongs to Axum — developers write standard Axum handlers, not Djogi abstractions. The rendering layer is the developer's choice — Dioxus, Askama, Maud, or whatever fits. Djogi merges app routers at startup and gets out of the way.

This is a deliberate architectural choice, not a missing feature. In an agentic workflow, routing and rendering are commoditized — AI generates handlers and templates trivially. The data layer is where framework-level consistency actually pays off: migration diffing, type-safe queries, JSONB schema preservation, audit logging. That is Djogi's scope.

**Design north star:** Define the model once, derive everything else — but make every derivation explicit and typed. No hidden middleware chains, no implicit magic, no convention-as-code. Typed and explicit, but never unnecessarily verbose.

**Postgres only.** Djogi targets PostgreSQL exclusively. This is not a temporary limitation — it is a permanent design decision. Postgres provides the features Djogi's derivation chain depends on: `JSONB` with path operators for typed schema fields, database-native ID generation via `generate_id()` (HeeRanjId), advisory locks for migration safety, `RETURNING` clauses, rich constraint system, and transactional DDL. Abstracting over multiple databases would mean giving up these capabilities or reimplementing them poorly. Every query Djogi generates, every migration it emits, every `Jsonb<T>` filter it compiles targets Postgres directly — no lowest-common-denominator SQL.

### 1.1 Djogi's Lane — What It Owns and What It Doesn't

Djogi is a Model-first framework. It owns the seam between Rust structs and the database — narrow in scope, deep within that scope. It does not duplicate or compete with what the underlying ecosystem already does well.

| Concern | Owner | Djogi's role |
|---|---|---|
| HTTP routing, middleware, request/response | Axum | Wires registered app routers at startup — nothing more |
| DB driver, connection pooling, row mapping | SQLx | Djogi wraps SQLx into a typed ORM layer (`Model`, `QuerySet`, `FromRow`, `ConditionBuilder`) but never hides it — raw `sqlx::QueryBuilder` is always an escape hatch |
| ID generation | HeeRanjId | Calls `generate_id()` / `generate_ids(n)` / `generate_ranj_id()` — owns nothing else |
| Async runtime | Tokio | Uses it; does not configure it |
| Model ↔ DB mapping | **Djogi** | Proc macro, field types, QuerySet, relations |
| Migrations | **Djogi** | Differ, generation, snapshot management |
| Shell | **Djogi** | Rhai REPL, model bindings, transaction control |
| CRUD logging | **Djogi** | Audit trail, JSON-aware diffing |
| JSONB schema fields | **Djogi** | `Jsonb<T>` — typed schemas over JSONB columns |
| Admin panel | **Djogi** (opt-in) | Auto-generated from `ModelDescriptor`; Dioxus fullstack; `admin` feature flag |

Anything outside that list is not Djogi's problem. The framework should never wrap, re-implement, or fight with Axum, SQLx, or HeeRanjId.
---

## 2. Core Dependencies

| Dependency | Role | Notes |
|---|---|---|
| `axum` | HTTP layer, routing, middleware | Core — not abstracted away from developer |
| `sqlx` | Async DB driver + migration runner | Postgres only |
| `tokio` | Async runtime | Standard |
| `serde` / `serde_json` | Serialization | Model ↔ JSON for shell and API |
| `rhai` | Embedded scripting shell | Rust-adjacent syntax, sandboxable |
| `figment` | Layered configuration | env vars + `Djogi.toml` |
| `clap` | CLI subcommands | `cargo djogi migrate`, etc. |
| `inventory` | Compile-time app/model registration | Cross-crate model discovery |
| `syn` + `quote` + `proc-macro2` | Proc macro infrastructure | Powers `#[derive(Model)]` |
| `time` | Datetime field types | Preferred over chrono — cleaner API, no CVE history |
| `heeranjid` | Primary key generation | External crate — HeerId (64-bit) and RanjId (128-bit UUIDv8) time-ordered IDs (see §3) |

**Explicitly excluded:**

- SeaORM / SeaQuery — replaced by Djogi's own query builder; would create a conceptual conflict
- Diesel — too compile-time-heavy, wrong ergonomic tradeoffs
- `chrono` — replaced by `time`
- Random UUID (v4) as default PK — replaced by HeerId; RanjId (UUIDv8, time-ordered) available as opt-in upgrade (see §3)
- App-code Snowflake generation — HeerId generates IDs at the database level
---

## 3. Primary Keys — HeeRanjId

Djogi uses the HeeRanjId system for primary key generation. HeeRanjId is maintained as a standalone external crate — it is not part of Djogi itself. This separation keeps the ID system independently maintainable, testable, and usable outside of Djogi.

HeeRanjId provides two related ID formats with a built-in upgrade path:

- **HeerId** (default): Compact 64-bit `BIGINT`, time-ordered, suitable for most applications
- **RanjId**: UUIDv8-compatible 128-bit identifier with sub-millisecond precision and higher node/sequence capacity — for when HeerId's headroom is insufficient

The migration from HeerId to RanjId is lossless: HeerId converts to RanjId without data loss, and the `UUID` column type is compatible with existing tooling. Start with HeerId; upgrade to RanjId when you need more headroom.

### 3.1 What HeerId Is

HeerId is a database-native 64-bit time-ordered identifier. IDs are generated by a Postgres function (`generate_id()`) installed by HeeRanjId's migration, not in application code. The column default is:

```sql
id BIGINT PRIMARY KEY DEFAULT generate_id()
```

Bit layout:
```
[41 bits: ms timestamp] [9 bits: node_id] [13 bits: sequence]
```

~69 years of timestamps, 512 distinct nodes, 8192 IDs per millisecond per node.

### 3.1a What RanjId Is

RanjId is a UUIDv8-compatible 128-bit identifier for applications that outgrow HeerId's capacity:

```sql
id UUID PRIMARY KEY DEFAULT generate_ranj_id()
```

Bit layout:
```
[48 bits: timestamp] [4 bits: version] [15 bits: node_id] [2 bits: variant] [13+bits: sequence]
```

Sub-millisecond precision, up to 32,767 nodes, 65,535 IDs per node per timestamp unit. Stored as a standard Postgres `UUID` — compatible with existing UUID tooling and indexes.

Opt in per model:
```rust
#[derive(Model)]
#[model(table = "high_volume_events", pk = "ranjid")]
pub struct Event {
    pub kind: String,
    pub payload: Jsonb<EventPayload>,
}
```
HeerId tables (installed once per database, managed by the `heer-id` crate):
```sql
CREATE TABLE heer_nodes (
    node_id       INTEGER PRIMARY KEY,
    name          TEXT NOT NULL,
    description   TEXT,
    is_active     BOOLEAN DEFAULT true,
    created_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_accessed TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE heer_config (
    id        INTEGER PRIMARY KEY CHECK (id = 1),
    epoch     TIMESTAMP NOT NULL,           -- custom epoch per deployment
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE heer_node_state (
    node_id       INTEGER PRIMARY KEY REFERENCES heer_nodes(node_id) ON DELETE CASCADE,
    last_id_time  BIGINT NOT NULL DEFAULT 0,
    last_sequence SMALLINT NOT NULL DEFAULT 0,
    updated_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
```
### 3.2 Node Identity

Node identity is environment-driven, not framework-driven:
```bash
NODE_ID=1
```
On startup, Djogi validates that `NODE_ID` exists in `heer_nodes` and fails fast if it does not. Nodes are treated as infrastructure — pinned per service instance, registered in `heer_nodes` as part of deployment.
Dynamic ephemeral nodes can lease/release IDs from `heer_nodes` using HeerId's session API:
```sql
set_heer_node_id(node_id INTEGER);
current_heer_node_id() RETURNS INTEGER;
```
### 3.3 JSON Serialization

JavaScript loses precision on 64-bit integers. `HeerId` always serializes as a string in JSON. Stored as `BIGINT` in Postgres, exposed as `"7493920192847234"` in API responses. This is enforced by HeerId's serde implementation — no per-field annotation required.
### 3.4 Serial as Opt-In

Small lookup/reference tables can opt into `SERIAL` where human-readable sequential IDs are genuinely useful:
```rust
#[derive(Model)]
#[model(table = "fuel_types", pk = "serial")]
pub struct FuelType {
    pub name: String,
}
```
### 3.5 ID Generation Patterns

HeerId supports three patterns. All are valid — the right choice depends on the use case.

**Pattern 1 — Default (zero ceremony):**

The `id` column default calls `generate_id()`. Djogi fires the INSERT and reads the ID back via `RETURNING id`. The developer never thinks about ID generation.
```rust
let car = Vehicle::create(&pool, Vehicle {
    make: "Toyota".into(),
    model_name: "Camry".into(),
    gas_fill: 50,
    active: true,
}).await?;

println!("{}", car.id);   // populated by RETURNING id
```
**Pattern 2 — Bulk pre-allocation (link tables and bulk relational inserts):**

Call `generate_ids(n)` to allocate a batch of IDs before any INSERT fires. All IDs are known upfront — cross-referencing related records in a bulk insert becomes straightforward. Exactly one update to `heer_node_state` for the entire batch, making it efficient and concurrency-safe.
```rust
// Allocate IDs before any write touches the DB
let ids = HeerId::generate_many(&pool, 3).await?;

let memberships = vec![
    PersonGroup { id: ids[0], person_id: alice.id, group_id: group.id, role: "admin".into() },
    PersonGroup { id: ids[1], person_id: bob.id,   group_id: group.id, role: "member".into() },
    PersonGroup { id: ids[2], person_id: carol.id, group_id: group.id, role: "member".into() },
];

PersonGroup::bulk_create(&pool, memberships).await?;
```
In the shell:
```rhai
let ids = HeerId::generate_many(3);
// build and inspect records before any write
```
**Pattern 3 — Form pre-generation (admin and create forms):**

Generate the ID at form render time — before the user submits anything. This solves a common pain point in frameworks where the ID isn't available until after the first INSERT, making file uploads and related records awkward to anchor.

With pre-generation:
- The ID is a hidden field from the moment the form renders
- File uploads can reference it before the record exists
- Related records can be staged against it
- The final INSERT becomes idempotent — retry or double-submit with the same ID is safe via `ON CONFLICT (id) DO NOTHING`
```rust
async fn new_vehicle_form(
    State(pool): State<PgPool>,
) -> impl IntoResponse {
    // ID generated at form render — before any user input
    let id = HeerId::generate(&pool).await?;

    Html(render_template("vehicles/new.html", context! {
        form_id: id,
    }))
}

async fn create_vehicle(
    State(pool): State<PgPool>,
    Form(input): Form<CreateVehicleInput>,
) -> impl IntoResponse {
    // ID arrives from the form — INSERT is idempotent
    Vehicle::create_with_id(&pool, input.form_id, Vehicle {
        make: input.make,
        model_name: input.model_name,
        gas_fill: input.gas_fill,
        active: true,
    }).await?;
}
```
The SQL underneath:
```sql
INSERT INTO vehicles (id, make, model_name, gas_fill, active, created_at, updated_at)
VALUES ($1, $2, $3, $4, $5, now(), now())
ON CONFLICT (id) DO NOTHING;
```
This is documented as a first-class Djogi idiom — idempotent writes with pre-generated IDs.
### 3.6 Clock and Sequence Edge Cases

HeerId handles these at the database level — Djogi does not need to think about them:

- **Minor clock drift (< 50ms):** HeerId returns an error; application layer retries
- **Major clock drift (> 50ms):** Hard fail — NTP slew mode is recommended in deployment
- **Sequence overflow:** Timestamp advanced +1ms, sequence reset to 0
- **Bulk generation:** Exactly one `heer_node_state` update for the entire batch regardless of size

---

## 4. The Model System

### 4.1 Defining a Model

The developer defines only the fields they own. Framework fields (`id`, `created_at`, `updated_at`) are injected by the macro — they are real struct fields and real DB columns, just not written by hand:
```rust
use djogi::prelude::*;

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[model(table = "vehicles")]
pub struct Vehicle {
    pub make: String,
    pub model_name: String,
    pub gas_fill: i32,
    pub active: bool,
}
```
After macro expansion the struct effectively becomes:
```rust
pub struct Vehicle {
    // Injected by macro — real fields, real DB columns
    pub id: HeerId,
    pub created_at: DateTime,
    pub updated_at: DateTime,

    // Developer-defined
    pub make: String,
    pub model_name: String,
    pub gas_fill: i32,
    pub active: bool,
}
```
`id` is generated by `generate_id()` at the database level — populated via `RETURNING id` after the INSERT. `created_at` and `updated_at` are DB defaults managed by the framework.
### 4.2 The Model Trait

`#[derive(Model)]` generates an implementation of the `Model` trait for the struct. The trait defines the full behavior contract:
```rust
pub trait Model: Sized {
    fn table_name() -> &'static str;
    fn descriptor() -> ModelDescriptor;

    fn objects() -> QuerySet<Self>;
    fn get(pool: &PgPool, id: HeerId) -> impl Future<Output = Result<Self>>;
    fn create(pool: &PgPool, value: Self) -> impl Future<Output = Result<Self>>;
    fn save(&self, pool: &PgPool) -> impl Future<Output = Result<()>>;
    fn delete(self, pool: &PgPool) -> impl Future<Output = Result<()>>;
}
```
`create()` takes the struct directly — no separate `CreateVehicle` wrapper. Framework-injected fields are ignored on input and populated by the framework before the `INSERT`.
### 4.3 What the Macro Generates

- Struct field injection (`id`, `created_at`, `updated_at`)
- `impl Model for Vehicle`
- `impl FromRow for Vehicle` — sqlx row deserialization
- `VehicleFields` — typed field accessors for the closure query builder
- `VehicleFilter` — programmatic filter builder for shell and dynamic use
- `Vehicle::descriptor()` — for app registration and migration differ

### 4.4 Field Types

| Rust Type | SQL Type (Postgres) |
|---|---|
| `String` | `TEXT` |
| `i32` | `INTEGER` |
| `i64` | `BIGINT` |
| `f64` | `DOUBLE PRECISION` |
| `bool` | `BOOLEAN` |
| `DateTime` (djogi wrapper over `time`) | `TIMESTAMPTZ` |
| `HeerId` | `BIGINT NOT NULL DEFAULT generate_id()` (injected PK) |
| `ForeignKey<T>` | `BIGINT REFERENCES table(id)` |
| `Jsonb<T>` | `JSONB` — typed schema with unknown field preservation |
| `Option<T>` | nullable version of T's SQL type |

### 4.5 Field Annotations
```rust
#[field(unique)]
pub email: String,

#[field(index)]
pub slug: String,

#[field(max_length = 100)]          // emits CHECK constraint
pub slug: String,

pub bio: Option<String>,            // Option<T> implies nullable

#[field(on_delete = "cascade")]
pub owner_id: ForeignKey<Owner>,

#[field(renamed_from = "old_name")] // informs differ: rename not drop+add
pub new_name: String,
```
---

## 5. The Query API

### 5.1 Await Strategy

In application code, `.await` is required and explicit — standard Rust async, no surprises.

In the shell, all terminal methods block transparently via an internal `block_on`. No `.await`, no async ceremony. The developer writes the same API in both contexts; the shell just removes the noise.

### 5.2 Instance Operations
```rust
// Fetch by PK
let mut car = Vehicle::get(&pool, id).await?;

// Mutate and persist
car.gas_fill = 70;
car.save(&pool).await?;

// Delete
car.delete(&pool).await?;

// Create — struct is the input, framework populates id/created_at/updated_at
let car = Vehicle::create(&pool, Vehicle {
    make: "Toyota".into(),
    model_name: "Camry".into(),
    gas_fill: 50,
    active: true,
    ..Default::default()    // framework fields — ignored on input
}).await?;
```
### 5.3 QuerySet — Lazy Builder

`QuerySet<T>` accumulates filters and options. Nothing hits the database until a terminal method is called.
```rust
let results = Vehicle::objects()
    .filter(|f| f.gas_fill.gte(69))
    .order_by(|f| f.gas_fill.desc())
    .limit(10)
    .fetch_all(&pool).await?;
```
QuerySets are cheap to clone and compose:
```rust
let active = Vehicle::objects().filter(|f| f.active.eq(true));

let cheap = active.clone().filter(|f| f.price.lte(20_000)).fetch_all(&pool).await?;
let fast  = active.clone().filter(|f| f.horsepower.gte(300)).fetch_all(&pool).await?;
```
### 5.4 Field Condition Methods

| Method | SQL equivalent |
|---|---|
| `.eq(val)` | `= $n` |
| `.neq(val)` | `!= $n` |
| `.gte(val)` | `>= $n` |
| `.gt(val)` | `> $n` |
| `.lte(val)` | `<= $n` |
| `.lt(val)` | `< $n` |
| `.in_list(vals)` | `IN ($n, ...)` |
| `.is_null()` | `IS NULL` |
| `.contains(s)` | `ILIKE '%s%'` |
| `.starts_with(s)` | `ILIKE 's%'` |
| `.between(a, b)` | `BETWEEN $n AND $m` |

Conditions are combinable inline:
```rust
.filter(|f| f.gas_fill.gte(50).and(f.active.eq(true)))
.filter(|f| f.make.eq("Toyota").or(f.make.eq("Honda")))
```
### 5.5 Programmatic Filter API

For dynamic construction and shell use where closures are unavailable:
```rust
let filter = VehicleFilter::new()
    .gas_fill(Gte(69))
    .active(Eq(true));

Vehicle::objects()
    .filter_struct(filter)
    .fetch_all(&pool).await?;
```
### 5.6 Underlying Engine — Native `ConditionBuilder` over `sqlx::QueryBuilder`

`QuerySet<T>` compiles its `Condition` tree into SQL via Djogi's own internal `ConditionBuilder`, a thin wrapper over `sqlx::QueryBuilder<Postgres>`. The framework does not depend on any third-party query-building crate — this layer is owned entirely by Djogi.

> **Design reference**: The community crate [`sqlx_clean_querybuilder`](https://github.com/this-ILECY/sqlx-clean-querybuilder) (MIT) serves as a reference implementation whose patterns were studied and adapted. It is not taken as a dependency — owning this code directly keeps Djogi's dependency surface lean and eliminates upstream risk for infrastructure-critical behavior.

| Layer | What it does |
|---|---|
| `QuerySet<T>` + filter closures | Developer-facing API; accumulates a typed `Condition` tree |
| `Condition` → `ConditionBuilder` | Djogi-internal: walks the tree, emits `push`/`push_bind` calls with correct `$n` numbering |
| `sqlx::QueryBuilder<Postgres>` | Manages the raw SQL buffer and positional parameter slots |
| `sqlx::query_as::<_, T>()` | Executes the built query and deserializes rows into the model type |

Developers can always drop down to raw `sqlx::QueryBuilder` directly for queries that exceed the `QuerySet` surface — Djogi is not a leaky abstraction that hides its plumbing:
```rust
use sqlx::QueryBuilder;

let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
    "SELECT * FROM vehicles WHERE make = "
);
qb.push_bind("Toyota");
qb.push(" AND gas_fill > ");
qb.push_bind(50);

let results: Vec<Vehicle> = qb.build_query_as().fetch_all(&pool).await?;
```
---

## 6. JSONB Schema Fields — `Jsonb<T>`

`Jsonb<T>` is a first-class Djogi field type that combines Postgres's `JSONB` column with a Rust schema type, serde deserialization, and validator-based validation. Unlike bare JSON fields that store and retrieve without any schema awareness, `Jsonb<T>` enforces typed schemas while preserving unknown fields.
### 6.1 Defining a JSONB Field
```rust
use djogi::prelude::*;

#[derive(JsonSchema, Serialize, Deserialize, Validate)]
pub struct TurboSpec {
    pub boost_psi: f64,
    #[validate(length(min = 1))]
    pub manufacturer: String,
}

#[derive(JsonSchema, Serialize, Deserialize, Validate)]
pub struct EngineSpec {
    pub cylinders: i32,
    #[validate(range(min = 0, max = 2000))]
    pub horsepower: i32,
    pub turbo: Option<Jsonb<TurboSpec>>,   // nested schema — fully typed
}

#[derive(Model)]
pub struct Vehicle {
    pub make: String,
    pub engine: Jsonb<EngineSpec>,         // JSONB column in Postgres
}
```
`Jsonb<T>` requires `T: JsonSchema + Serialize + DeserializeOwned + Validate`.
Nested `Jsonb<T>` is fully supported — each level of nesting has its own typed schema with its own known/unknown field boundary. There is no depth limit.
### 6.2 Internal Layout
```rust
pub struct Jsonb<T> {
    pub data: T,                            // fully typed, validated on save
    extra: IndexMap<String, UnknownField>,  // unknown fields — preserved, never dropped
}
```
### 6.3 Unknown Field Preservation

Fields present in the stored JSON but absent from the schema are never dropped on save. They are loaded into `extra`, accessible at runtime, and round-tripped untouched through every `save()`.

Given this JSON in the database:
```json
{
  "cylinders": 8,
  "horsepower": 450,
  "turbo": {
    "boost_psi": 18.5,
    "manufacturer": "Garrett",
    "part_number": "GT3582R"
  },
  "legacy_ecu_code": "M62B44"
}
```
- `cylinders`, `horsepower`, `turbo` — known fields, fully typed via `EngineSpec`
- `turbo.part_number` — unknown at `TurboSpec` level, preserved in `TurboSpec`'s `extra`
- `legacy_ecu_code` — unknown at `EngineSpec` level, preserved in `EngineSpec`'s `extra`

On save, all unknown fields are written back exactly as loaded. No data is ever silently destroyed.

### 6.4 The `UnknownField` Type

Unknown fields surface as a runtime-typed enum with a fixed, honest set of variants. Nested unknown objects and arrays are not recursed into — they surface as `RawJson` to keep the API boundary clean and avoid becoming a dynamic JSON library.
```rust
pub enum UnknownField {
    String(String),
    Bool(bool),
    Float(f64),
    Int(i64),
    Null,
    RawJson(String),    // unknown nested object or array — raw JSON string
}
```
### 6.5 Accessing Unknown Fields — `UnknownFieldError`

All conversions return `Result` — never `Option` or a raw panicking value. Implicit coercion between types is never performed. A string that looks like a number is not silently converted.
```rust
pub enum UnknownFieldError {
    // Field key does not exist at this schema level
    FieldNotFound { field: String },
    // Asked for f64, it is stored as String
    TypeMismatch { field: String, expected: &'static str, actual: &'static str },
    // String value that looks like the requested type — coercion refused
    NoImplicitCoercion { field: String, value: String, into: &'static str },
}
```
Examples:
```rust
// DB has: "legacy_ecu_code": "M62B44"
car.engine.extra("legacy_ecu_code")?.as_str()
// Ok("M62B44")

car.engine.extra("legacy_ecu_code")?.as_f64()
// Err(TypeMismatch { field: "legacy_ecu_code", expected: "f64", actual: "String" })

// DB has: "boost_psi": "18.5"  ← string, not float — data quality problem
turbo.extra("boost_psi")?.as_f64()
// Err(NoImplicitCoercion { field: "boost_psi", value: "18.5", into: "f64" })
// Not silently Ok(18.5) — the caller must fix the data

// Field does not exist at this level
car.engine.extra("nonexistent")
// Err(FieldNotFound { field: "nonexistent" })

// Nested unknown traversal
car.engine.data.turbo
    .as_ref()
    .and_then(|t| t.extra("part_number").ok())
    .and_then(|f| f.as_str().ok())
// Some("GT3582R")

// Inspect all unknown fields at a level
for (key, val) in car.engine.unknown_fields() {
    println!("{}: {:?}", key, val);
}
```
### 6.6 Validation on Save

Validation runs through the full schema tree before any write touches the database. If any level fails, the save is aborted with a structured error — nothing is written.
```rust
car.engine.data.horsepower = 5000;  // exceeds range(max = 2000)
car.save(&pool).await?;
// Err: validation failed: engine.horsepower must be <= 2000

car.engine.data.turbo = Some(Jsonb::new(TurboSpec {
    boost_psi: 18.5,
    manufacturer: "".into(),   // violates length(min = 1)
}));
car.save(&pool).await?;
// Err: validation failed: engine.turbo.manufacturer must not be empty
```
Error paths use dot-notation through the full nesting depth so the developer knows exactly where the failure is.
### 6.7 Subfield Query Filters

The proc macro generates typed filter accessors for all known fields at every nesting level, using Postgres's JSONB path operators.
```rust
// Known field at root level
Vehicle::objects()
    .filter(|f| f.engine.horsepower.gte(300))
    // WHERE (engine->>'horsepower')::integer >= 300

// Known field in nested schema
Vehicle::objects()
    .filter(|f| f.engine.turbo.boost_psi.gte(15.0))
    // WHERE (engine->'turbo'->>'boost_psi')::float >= 15.0

Vehicle::objects()
    .filter(|f| f.engine.turbo.manufacturer.eq("Garrett"))
    // WHERE engine->'turbo'->>'manufacturer' = 'Garrett'
```
Unknown fields cannot be used in typed filter closures — they are not known at compile time. Raw SQL via `.raw_filter()` is the escape hatch for querying unknown fields.
### 6.8 Shell Access
```rhai
let car = Vehicle::get(42);

// Known fields — typed, direct
print(car.engine.horsepower);
print(car.engine.turbo.boost_psi);

// Unknown fields — Result-returning, explicit
print(car.engine.extra("legacy_ecu_code").as_str());
print(car.engine.turbo.extra("part_number").as_str());

// Inspect all unknowns
pp(car.engine.unknown_fields());

// Filter by nested known field
let powerful = Vehicle::objects()
    .filter_struct(VehicleFilter::new().engine_horsepower(Gte(300)))
    .fetch_all();
```
---

## 7. Dirty Tracking (Feature Flag: `dirty-tracking`)

When enabled, fetched models track which fields changed. `save()` issues `UPDATE` only for dirty fields.
```rust
let mut car = Vehicle::get(&pool, id).await?;
car.gas_fill = 80;
// Emits: UPDATE vehicles SET gas_fill = $1, updated_at = $2 WHERE id = $3
car.save(&pool).await?;
```
The macro injects a hidden `__dirty: DirtyTracker` field. Off by default — opt in globally via `Djogi.toml` or per-model with `#[model(dirty_tracking)]`.
---

## 8. Relations

### 8.1 ForeignKey
```rust
#[derive(Model)]
#[model(table = "vehicles")]
pub struct Vehicle {
    pub make: String,
    #[field(on_delete = "cascade")]
    pub owner_id: ForeignKey<Owner>,
    pub fuel_type_id: ForeignKey<FuelType>,   // defaults to RESTRICT
}
```
Eager loading — explicit, no lazy loading:
```rust
// Single fetch — one query, explicit
let owner = car.owner_id.fetch(&pool).await?;

// Prefetch on QuerySet — one IN (...) query per relation, not N+1
let cars = Vehicle::objects()
    .prefetch(VehicleRelated::owner())
    .prefetch(VehicleRelated::fuel_type())
    .fetch_all(&pool).await?;

let owner = cars[0].owner_id.resolved();   // -> Option<&Owner>, free after prefetch
```
No lazy loading. No surprise queries. The developer always knows when the DB is hit.
### 8.2 Many-to-Many — Explicit Through Models

Implicit M2M fields are not provided. All M2M relationships require an explicit through model — this avoids the forced migration that implicit M2M fields eventually require when you need to store data on the relationship.
```rust
#[derive(Model)]
pub struct Person {
    pub name: String,
}

#[derive(Model)]
pub struct Group {
    pub name: String,
}

#[derive(Model)]
#[model(table = "person_groups", through)]
pub struct PersonGroup {
    pub person_id: ForeignKey<Person>,
    pub group_id: ForeignKey<Group>,
    pub joined_at: DateTime,
    pub role: String,
}
```
Declaring the relationship — both directions are explicit:
```rust
impl ManyToMany<Group> for Person {
    type Through = PersonGroup;
    const RELATION: &'static str = "groups";    // generates person.groups()
}

impl ManyToMany<Person> for Group {
    type Through = PersonGroup;
    const RELATION: &'static str = "members";   // generates group.members()
}
```
Explicit naming via `RELATION` is intentional — auto-pluralization is error-prone and the developer's domain language is more expressive (`members` vs `persons`).
Generated convenience methods:
```rust
// Person side
let groups = person.groups(&pool).await?;
person.add_to_group(&pool, &group, PersonGroup {
    role: "admin".into(), ..Default::default()
}).await?;
person.remove_from_group(&pool, &group).await?;

// Group side
let members = group.members(&pool).await?;
group.add_to_member(&pool, &person, PersonGroup { ... }).await?;
group.remove_from_member(&pool, &person).await?;

// Through model is a full Model — directly queryable
let admins = PersonGroup::objects()
    .filter(|f| f.role.eq("admin"))
    .fetch_all(&pool).await?;
```
---

## 9. Automated Audit & Event Logging — Two Parallel Databases

Djogi separates all logging concerns across two purpose-built databases, each isolated from application data. The framework maintains three concurrent connection pools at startup: application (`url`), CRUD audit (`crud_log_url`), and event/observability (`event_log_url`).

| Database | Config Key | Purpose | Typical Retention |
|---|---|---|---|
| `myapp` | `url` | Application data | Permanent |
| `myapp_crud_logs` | `crud_log_url` | Structural CRUD audit trail | Long-term / compliance |
| `myapp_event_logs` | `event_log_url` | Request, crash, and debug events | Short-to-medium term |

The separation means CRUD logs from two years ago live untouched regardless of how aggressively event logs from last week are pruned.

### 9.1 CRUD Log Database — Structural Audit Trail

Every model with CRUD logging enabled automatically provisions a mirror `_logs` table in the CRUD Log Database (e.g. `Vehicle` → `vehicle_logs` in `myapp_crud_logs`). After any successful `INSERT`, `UPDATE`, or `DELETE`, the framework asynchronously writes the before/after snapshot and a `JSONB` diff to this database.
Enabling:
```toml
# Djogi.toml
[database]
url          = "postgres://localhost/myapp"
crud_log_url = "postgres://localhost/myapp_crud_logs"
event_log_url = "postgres://localhost/myapp_event_logs"

[features]
crud_log = false   # global default — opt in per model or globally
```
```rust
// Per model
#[derive(Model)]
#[model(table = "vehicles", crud_log = true)]
pub struct Vehicle { ... }

// Opt out a specific model when globally enabled
#[derive(Model)]
#[model(table = "internal_tokens", crud_log = false)]
pub struct InternalToken { ... }
```
The mirror log table schema (auto-provisioned per model):
```sql
CREATE TABLE vehicle_logs (
    id          BIGINT PRIMARY KEY DEFAULT generate_id(),
    record_id   BIGINT NOT NULL,
    event       TEXT NOT NULL CHECK (event IN ('created', 'updated', 'deleted')),
    changes     JSONB,               -- array of FieldChange — null for created/deleted
    snapshot    JSONB,               -- full record snapshot for created/deleted
    actor       TEXT,                -- optional — who made the change
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_vehicle_logs_record ON vehicle_logs (record_id);
CREATE INDEX idx_vehicle_logs_occurred_at ON vehicle_logs (occurred_at);
```
JSON-aware diffing — changes to both regular model fields and nested `Jsonb<T>` subfields are captured with full dot-notation path precision. Unknown fields that changed between load and save are also captured.
```rust
// CrudEvent::Updated written to vehicle_logs:
// changes: [
//   { path: "make",                    before: "Toyota", after: "Lexus"  },
//   { path: "engine.horsepower",       before: 450,      after: 500      },
//   { path: "engine.turbo.boost_psi",  before: 15.0,     after: 18.5    },
// ]
```
Actor attribution:
```rust
car.save_with_actor(&pool, "user:8312847293").await?;
// or via request-context hook — all saves in a handler attributed automatically
```
Querying from the shell:
```rhai
// Full history for a record — uses the crud_log pool automatically
let history = VehicleLog::objects()
    .filter(|f| f.record_id.eq(car.id))
    .order_by(|f| f.occurred_at.desc())
    .fetch_all();

pp(history);

// Changes to a specific nested field
let changes = VehicleLog::objects()
    .filter(|f| f.event.eq("updated"))
    .json_path_changed("engine.horsepower")
    .fetch_all();
```
### 9.2 Event Log Database — Observability & Tracing

All behavioral, observability, and system events are routed to the Event Log Database. This covers request logs, handler lifecycle, crash reports, and debug traces — anything that describes what the system did, not what data changed.

- **Tracing integration:** Built on `tracing` + `tracing-subscriber`. A background Layer routes structured spans and events into `_djogi_events` in the Event Log Database
- **Severity routing:** `DEBUG`/`INFO` spans go to the events table; `WARN`/`ERROR`/`CRITICAL` additionally fan out to Sentry, OpenTelemetry, or Datadog via standard subscriber layering — no custom adapters needed
- **Access pattern:** Semi-structured `JSONB` payloads, queried constantly during active debugging, naturally suited to shorter retention than CRUD logs

### 9.3 Log Lifecycle
```bash
# Wipe app DB only — both log databases untouched
cargo djogi db reset

# Wipe app DB and CRUD log DB — event logs retained
cargo djogi db reset --wipe-crud-logs

# Wipe all three databases
cargo djogi db reset --wipe-all-logs
```
`db reset` guards (`dev_mode`, localhost URL, `DJOGI_ENV`) apply to all variants.

## 10. Migrations

### 10.1 Philosophy

- Migration files are plain SQL — readable, editable, reviewable
- Execution is sqlx's built-in runner — checksummed, tracked in `_sqlx_migrations`
- Generation happens automatically at build time via `build.rs`
- `schema_snapshot.json` is the source of truth for the differ — updated only on successful `cargo djogi migrate`
- Up and down files always generated as a pair
- The `migrations/` folder is a git submodule — managed by the pipeline, invisible to the developer day-to-day

### 10.2 Build-Time Drift Detection and Generation

`build.rs` runs on every `cargo build`:

1. Reads model descriptors from `target/djogi_models.json` (written as a side-effect of proc macro expansion via `inventory`)
2. Diffs against `migrations/schema_snapshot.json` (the current DB state)
3. If drift is detected:
   - Generates a migration pair (`NNNN_description_up.sql` / `NNNN_description_down.sql`) into `migrations/`
   - Emits a compiler warning (not an error) with the generated filenames
   - Build proceeds — the developer reviews and applies when ready
4. If no drift: build proceeds silently
Drift notification looks like a compiler diagnostic:
```
warning[D001]: schema drift detected — migration generated
  --> src/apps/vehicles/models.rs:8:9
   |
 8 |     pub horsepower: i32,
   |     ^^^^^^^^^^^^^^^^^^^ new field — no migration existed
   |
   = note: generated migrations/0005_add_vehicles_horsepower_up.sql
   = note: generated migrations/0005_add_vehicles_horsepower_down.sql
   = help: review the SQL, then run `cargo djogi migrate` when ready
```
Destructive operations (DROP COLUMN, DROP TABLE) emit a warning and require `--allow-destructive` to proceed:
```
warning[D002]: destructive migration requires confirmation
   = help: run `cargo djogi makemigrations --allow-destructive` to generate
```
### 10.3 Schema Snapshot

`migrations/schema_snapshot.json` represents what the database actually looks like — updated only when `cargo djogi migrate` succeeds. Never updated by the build step.
```json
{
  "version": "0005",
  "migrated_at": "2025-03-26T10:00:00Z",
  "models": {
    "vehicles": {
      "columns": [
        { "name": "id", "sql_type": "BIGINT", "nullable": false, "primary_key": true },
        { "name": "owner_id", "sql_type": "BIGINT", "nullable": false,
          "references": { "table": "owners", "column": "id", "on_delete": "restrict" } },
        { "name": "gas_fill", "sql_type": "INTEGER", "nullable": false }
      ],
      "indexes": [{ "name": "idx_vehicles_active", "columns": ["active"] }]
    }
  }
}
```
### 10.4 Generated Migration Pair
`migrations/0005_add_vehicles_horsepower_up.sql`:
```sql
-- Migration: 0005_add_vehicles_horsepower
-- Direction: UP
-- Generated: 2025-03-26T10:00:00Z

ALTER TABLE vehicles ADD COLUMN horsepower INTEGER NOT NULL DEFAULT 0;
CREATE INDEX idx_vehicles_horsepower ON vehicles (horsepower);
```
`migrations/0005_add_vehicles_horsepower_down.sql`:
```sql
-- Migration: 0005_add_vehicles_horsepower
-- Direction: DOWN
-- WARNING: dropping a column is irreversible — data is not recoverable on rollback

DROP INDEX idx_vehicles_horsepower;
ALTER TABLE vehicles DROP COLUMN horsepower;
```
Down migration generation rules:

| Up | Down | Notes |
|---|---|---|
| `CREATE TABLE` | `DROP TABLE` | |
| `DROP TABLE` | `CREATE TABLE` | Full definition re-emitted |
| `ADD COLUMN` | `DROP COLUMN` | |
| `DROP COLUMN` | `ADD COLUMN` | ⚠ Data unrecoverable |
| `CREATE INDEX` | `DROP INDEX` | |
| `DROP INDEX` | `CREATE INDEX` | |
| `ALTER COLUMN` | `ALTER COLUMN` reverse | ⚠ Warn if lossy |
| `ADD FOREIGN KEY` | `DROP FOREIGN KEY` | |

### 10.5 Migrations Git Submodule

The `migrations/` directory is a git submodule. The pipeline owns it:
```yaml
ci:
  - cargo build              # generates any missing migration files
  - cargo djogi migrate      # applies SQL, updates schema_snapshot.json on success
  - cd migrations && git add . && git commit -m "migrate: $(date)" && git push
```
Locally the developer never touches the submodule directly. Migration files appear in their editor after `cargo build`, they review the SQL, then run `cargo djogi migrate` when ready.
### 10.6 Differ
```rust
enum SchemaDelta {
    CreateTable { table: TableDef },
    DropTable { name: String },
    AddColumn { table: String, column: ColumnDef },
    DropColumn { table: String, name: String },
    AlterColumn { table: String, name: String, change: ColumnChange },
    AddIndex { table: String, index: IndexDef },
    DropIndex { name: String },
    AddForeignKey { table: String, fk: ForeignKeyDef },
    DropForeignKey { table: String, name: String },
}
```
Field renames treated as drop+add unless annotated with `#[field(renamed_from = "old_name")]`.
---

## 11. App Registration
```rust
djogi::register_app!(VehiclesApp);

struct VehiclesApp;
impl App for VehiclesApp {
    fn models() -> &'static [ModelDescriptor] {
        &[Vehicle::descriptor(), PersonGroup::descriptor()]
    }
    fn routes() -> Router { vehicles_router() }
}
```
Apps are registered at link time via `inventory`. At startup Djogi collects all registered apps, merges their `Router`s, and makes all `ModelDescriptor`s available to the differ and shell.
---

## 12. Configuration

`Djogi.toml` at the project root:
```toml
[database]
url = "postgres://localhost/myapp"
max_connections = 10
dev_mode = false
# NODE_ID is set as an environment variable, not in Djogi.toml — it is infrastructure config

[server]
host = "0.0.0.0"
port = 8000

[migrations]
submodule = "migrations"
allow_destructive = false

[shell]
history_file = ".djogi_history"                # gitignored — personal and noisy
transaction_timeout_default = "30m"            # pre-fills the begin() prompt; developer can clear it
scripts_dir = "scripts"                        # committed, shareable shell scripts
error_log_dir = ".djogi_shell_errors"          # gitignored — full tracebacks on disk
error_log_retention = "1y"                    # auto-purge logs older than this on shell startup

[features]
dirty_tracking = false
```
`DATABASE_URL` env var always overrides `[database].url`. Secrets live in env vars, never in `Djogi.toml`.
---

## 13. The Shell

### 13.1 Invocation
```bash
cargo djogi shell
```
Starts a Rhai REPL with all registered models pre-loaded, a live database connection, and a persistent command history file.
### 13.2 Async Strategy

The shell holds a dedicated single-threaded `tokio` runtime. Every terminal method (`fetch_all`, `fetch_one`, `first`, `count`, `save`, `delete`, `create`, `get`, `fetch` on FK) wraps its async implementation in `runtime.block_on(...)` internally.
No `.await` anywhere in the shell. Blocking is a feature — the shell is for interactive exploration, not production throughput.
### 13.3 Error Handling

Errors print a clean one-liner in the shell and never unwind the session. Local variables, open transactions, and history all survive. The full traceback is written to `.djogi_shell_errors/` so it's available when needed without cluttering the prompt.
```
djogi> let car = Vehicle::get(99999)
Error: record not found (vehicles where id = 99999)
  → traceback saved to .djogi_shell_errors/2025-03-26T10-42-11_001.log

djogi> let car = Vehicle::get(42)
djogi> car.gas_fill = 90
djogi> car.save()
djogi>
```
Errors inside a transaction do not auto-rollback — that is always the developer's explicit choice. Where Postgres itself aborts the transaction (e.g. constraint violation), the shell notes it and waits:
```
djogi (txn)> car.owner_id = 99999
djogi (txn)> car.save()
Error: foreign key violation — owner_id 99999 does not exist in owners
  → traceback saved to .djogi_shell_errors/2025-03-26T10-42-33_002.log
Note: Postgres has aborted this transaction. Call rollback() to clear it.
djogi (txn)>
```
Error log format:
```
# .djogi_shell_errors/2025-03-26T10-42-33_002.log

Timestamp:   2025-03-26T10:42:33Z
Session:     started 2025-03-26T10:40:11Z
Transaction: open (began 2025-03-26T10:41:05Z)

Error:       foreign key violation
Message:     insert or update on table "vehicles" violates foreign key constraint
             "vehicles_owner_id_fkey"
Detail:      Key (owner_id)=(99999) is not present in table "owners"

Rhai stack:
  at save() [built-in]
  at line 3, col 1 in shell session

SQL attempted:
  UPDATE vehicles SET owner_id = $1, updated_at = $2 WHERE id = $3
  params: [99999, 2025-03-26T10:42:33Z, 42]
```
Full stack traces are enabled in all log files by default. Pass `--verbose` at shell startup to also print them inline for framework debugging.
Log files are auto-purged on shell startup based on `error_log_retention` (default: `1y`). Manual clear: `.clear_errors`.
### 13.4 Transactions in the Shell

The shell exposes explicit transaction control. Open transactions are clearly indicated in the prompt.
Starting a transaction prompts for an optional timeout — not enforced by the framework, but offered as a reminder. The `transaction_timeout_default` config value pre-fills the prompt; the developer can accept it, change it, or clear it entirely:
```
djogi> begin()
Transaction timeout [default: 30m, or enter duration e.g. 1h, or leave blank for none]: _
Note: uncommitted work will be lost if the shell exits or loses connection.
djogi (txn)>
```
Timeout is advisory — it's a Postgres `SET LOCAL statement_timeout` on the connection, not a framework timer. The developer who is mid-thought and steps away is not penalized unless they opted into a timeout. Defaulting to none would be fine; the config default just pre-fills a sensible suggestion.
```
djogi> begin()
Transaction timeout [default: 30m]: 
djogi (txn)> let car = Vehicle::get(42)
djogi (txn)> car.gas_fill = 0
djogi (txn)> car.save()
djogi (txn)> pp(Vehicle::get(42))    // inspect safely inside the transaction
djogi (txn)> commit()
djogi>

// Or change your mind
djogi (txn)> rollback()
djogi>
```
Power loss / process crash: Postgres handles this correctly at the protocol level. When the shell process dies — cleanly or not — the TCP connection drops and Postgres automatically rolls back any open transaction. No partial commits, no manual cleanup required. The developer reconnects via `cargo djogi shell` and the database is exactly as it was before `begin()`.
Closing the shell with an open transaction always triggers an explicit `ROLLBACK` before exit — never a silent commit.
Savepoints for complex sessions:
```rhai
begin();
savepoint("checkpoint");
// risky work
rollback_to("checkpoint");
// try something else
commit();
```
### 13.5 Shell Capabilities
```rhai
// Query — no .await
let cars = Vehicle::objects()
    .filter_struct(VehicleFilter::new().gas_fill(Gte(69)).active(Eq(true)))
    .fetch_all();

pp(cars);                            // ASCII table
print(cars[0].make);

// FK traversal
let owner = cars[0].owner_id.fetch();

// M2M
let groups = person.groups();
person.add_to_group(group, #{ role: "admin" });
person.remove_from_group(group);

let members = group.members();

// Mutate
let car = Vehicle::get(42);
car.gas_fill = 90;
car.save();

// Create
let car = Vehicle::create(#{ make: "Ford", model_name: "F-150", gas_fill: 60, active: true });

// Delete
car.delete();

// Raw SQL
let rows = sql("SELECT make, COUNT(*) FROM vehicles GROUP BY make");
pp(rows);
```
### 13.6 Shell Utilities

| Helper | Description |
|---|---|
| `pp(value)` | ASCII table (collections) or key-value (single model) |
| `sql("...")` | Raw SQL — returns array of dynamic maps |
| `begin()` | Start a transaction, prompts for optional timeout |
| `commit()` | Commit open transaction |
| `rollback()` | Rollback open transaction |
| `savepoint("name")` | Create a savepoint |
| `rollback_to("name")` | Rollback to savepoint |
| `.export name` | Save current session history to `scripts/name.rhai` |
| `.export name --from bookmark` | Save from a named bookmark position |
| `.bookmark name` | Bookmark current history position |
| `.import name` | Run `scripts/name.rhai` inside the current session |
| `reload()` | Re-initialize model bindings mid-session |
| `.clear_errors` | Delete all logs in `.djogi_shell_errors/` |

### 13.7 Session Import / Export
Shell sessions can be saved as named Rhai scripts and replayed later or shared with teammates.
Export writes the current session's meaningful history to `scripts/`:
```
djogi> .export analysis_q3_vehicles
Saved to scripts/analysis_q3_vehicles.rhai
```
Raw navigation (up-arrow corrections, typos) is filtered out. The developer can open the file, clean it up, and commit it.
Bookmark a mid-session position to export from a specific point:
```
djogi> .bookmark before_delete
djogi> .export backfill_owners --from before_delete
Saved to scripts/backfill_owners.rhai
```
Import / run inside the REPL:
```
djogi> .import analysis_q3_vehicles
Running scripts/analysis_q3_vehicles.rhai...
djogi>
```
Headless run without entering the REPL:
```bash
cargo djogi shell --run scripts/analysis_q3_vehicles.rhai
```
Scripts run in the full shell environment with access to all models. They are useful beyond history replay — lightweight data analysis, one-off backfills, or team-shared query libraries.
Gitignore convention:
```
.djogi_history            # gitignored — personal, noisy
.djogi_shell_errors/      # gitignored — full tracebacks, retained for 1y by default
scripts/                  # committed — curated, shareable
seeds.rhai                # committed — project seed data
```
### 13.8 Seed Scripts
`seeds.rhai` at project root runs in the full shell environment:
```bash
cargo djogi db seed
```
Uses the same model API the developer already knows.
---

## 14. CLI — `cargo djogi`

Installed once, used everywhere:
```bash
cargo install djogi-cli
```
```bash
# Migrations
cargo djogi migrate                          # apply pending migrations, update snapshot
cargo djogi migrate rollback                 # roll back last migration, rewind snapshot
cargo djogi migrate --fake 0005             # mark applied without running SQL

# Migration generation (manual trigger — build.rs handles automatic generation)
cargo djogi makemigrations                   # force-generate from current drift
cargo djogi makemigrations --dry-run         # preview SQL without writing files
cargo djogi makemigrations --allow-destructive

# Database (dev only — gated on dev_mode + localhost + DJOGI_ENV != production)
cargo djogi db reset                         # drop → recreate → migrate
cargo djogi db reset --seed                  # drop → recreate → migrate → seed
cargo djogi db seed                          # run seeds.rhai only

# Shell
cargo djogi shell

# Project scaffolding
cargo djogi new my-project                   # scaffold project + init migrations submodule
cargo djogi init                             # add Djogi to existing project
```
`db reset` hard-errors unless all three guards pass: `dev_mode = true`, `DATABASE_URL` resolves to localhost, `DJOGI_ENV` is not `production`.
---

## 15. Admin Panel — Dioxus ModelForms

Djogi ships an optional admin panel that auto-generates list views and CRUD forms for every registered model. Built with Dioxus 0.7+ and served as a fullstack Dioxus application mounted at `/_admin/` by the Axum router.

### 15.1 Design Philosophy

- **Zero hand-written UI:** `ModelDescriptor` (emitted by `#[derive(Model)]`) carries all the information needed — field names, types, nullability, FK targets, validation constraints. The admin reads this at runtime and renders forms automatically
- **Dioxus 0.7+ fullstack:** The admin runs as server-side rendered + hydrated WASM. Server functions (`#[server]`) call directly into the Djogi model layer on the backend — no separate REST API needed for admin operations
- **Opt-in, not bundled by default:** Enabled via the `admin` feature flag — adds Dioxus to the dependency tree only when explicitly requested
```toml
# Cargo.toml
djogi = { version = "0.1", features = ["admin"] }
```
### 15.2 Mounting the Admin

The admin router is merged automatically at startup when the `admin` feature is active:
```
GET  /_admin/                     → model index (all registered models)
GET  /_admin/{model}/             → list view + pagination + search
GET  /_admin/{model}/add/         → create form
GET  /_admin/{model}/{id}/        → detail / edit form
POST /_admin/{model}/{id}/        → save changes
POST /_admin/{model}/{id}/delete/ → delete with confirmation prompt
```
Access is gated behind configurable admin credentials:
```toml
[admin]
enabled  = true
path     = "/_admin/"
username = "admin"            # or DJOGI_ADMIN_USER env var
# password set via: cargo djogi admin set-password
```
### 15.3 List View — Pagination, Search, Filtering & Sorting

Every model's admin list view is paginated by default. All behaviour is driven from `ModelDescriptor` and tunable per model.

Defaults (zero configuration):
- Paginated at 25 rows per page
- Sortable by any column header (click to sort asc/desc)
- Global search box performs case-insensitive `ILIKE` across all `String` fields

Declarative configuration:
```rust
#[derive(Model)]
#[model(
    table = "vehicles",
    admin_list_display  = ["make", "model_name", "gas_fill", "active"],
    admin_search_fields = ["make", "model_name"],
    admin_filter_fields = ["active", "fuel_type_id"],
    admin_sort_default  = "created_at",
    admin_sort_default_dir = "desc",
    admin_page_size     = 50,
)]
pub struct Vehicle { ... }
```
Filter widgets auto-typed by field:

| Field type | Filter widget |
|---|---|
| `bool` | Toggle: All / True / False |
| `Option<T>` | Toggle: All / Set / Null |
| `ForeignKey<T>` | `<select>` populated by `T::objects()` |
| `DateTime` | Date range picker (from / to) |
| `i32` / `i64` / `f64` | Numeric range (min / max) |
| `#[field(choices = ...)]` | Checkbox group of allowed values |

All filter state, sort, and search are reflected in the URL query string — list views are bookmarkable and shareable.

### 15.4 ModelForms — Field Widget Mapping

For each registered model, the admin generates a form view by iterating `ModelDescriptor` at runtime. No per-model UI code is written by hand.

| Field type | Admin widget |
|---|---|
| `String` | `<input type="text">` |
| `Option<String>` | `<input type="text">` (clearable, nullable) |
| `i32` / `i64` / `f64` | `<input type="number">` |
| `bool` | `<input type="checkbox">` |
| `DateTime` | `<input type="datetime-local">` |
| `Option<T>` | Widget for `T` + explicit null toggle |
| `ForeignKey<T>` | `<select>` populated by `T::objects().fetch_all()` |
| `Jsonb<T>` | Nested fieldset generated from `T`'s schema — unknown fields shown as read-only |
| `#[field(max_length = N)]` | `maxlength` attribute on text input |
| `#[field(unique)]` | Client-side hint; uniqueness validated server-side on save |
### 15.5 Changeform Validation — Clean Before Save

Before any `INSERT` or `UPDATE` fires, the changeform runs a clean pipeline. Errors are surfaced inline, next to the offending field.

Auto-generated field validation (always active, derived from `ModelDescriptor`):
- `#[field(max_length = N)]` — trims to N chars, error if still over
- `#[field(unique)]` — queries DB for conflict before save
- Non-nullable fields — error if blank submitted
- `ForeignKey<T>` — verifies referenced row exists before save
- `Jsonb<T>` — runs `validator` validation on the schema before save

Custom `clean()` hook (opt-in, per model):
```rust
use djogi::admin::AdminClean;

impl AdminClean for Vehicle {
    async fn clean(&mut self, pool: &PgPool) -> Result<(), AdminValidationError> {
        // Cross-field validation
        if self.active && self.gas_fill == 0 {
            return Err(AdminValidationError::field(
                "gas_fill",
                "An active vehicle must have fuel.",
            ));
        }
        // Normalize before save
        self.make = self.make.trim().to_string();
        Ok(())
    }
}
```
`AdminValidationError` carries either a field-specific message or a form-level message. `clean()` runs after auto-validation passes and before `create()` or `save()`. If it returns `Err`, the save is aborted and the form re-rendered — no DB write occurs. Models without `impl AdminClean` skip the hook entirely.
### 15.6 M2M Through-Table Inlines

When editing a model with a `ManyToMany` relationship declared, the admin renders through-table rows as an inline sub-form beneath the main model form. Because Djogi requires explicit through models and `impl ManyToMany<T>` declarations, the admin has all the metadata it needs with zero extra configuration.
```
┌─ Person: Alice ──────────────────────────────────┐
│  name:  [Alice                    ]              │
│                                                  │
│  Groups (PersonGroup)                            │
│  ┌──────────────────┬───────────┬──────────────┐ │
│  │ Group            │ Role      │ Joined At    │ │
│  ├──────────────────┼───────────┼──────────────┤ │
│  │ [Engineering  ▼] │ [admin  ] │ [2026-01-01] │ │
│  │ [Product      ▼] │ [member ] │ [2026-03-01] │ │
│  │ [+ Add row      ]                            │ │
│  └──────────────────┴───────────┴──────────────┘ │
│  [ Save ]  [ Delete ]                            │
└──────────────────────────────────────────────────┘
```
- Each inline row maps the through model's fields to widgets using the same field → widget table
- `ForeignKey<T>` fields render as `<select>` dropdowns
- On save, the admin diffs the inline state against existing rows and fires the appropriate `create`/`delete` calls
- Inline pagination — at most `inline_page_size` rows shown (default: 10) with load-more; new unsaved rows always shown above the paginated set
- Inline search — filters visible rows client-side and fires server-side filtered queries on page load
```toml
[admin]
inline_page_size = 10
```
Configurable per relationship:
```rust
impl ManyToMany<Group> for Person {
    type Through = PersonGroup;
    const RELATION: &'static str = "groups";
    const ADMIN_INLINE_PAGE_SIZE: usize = 5;  // override for this relationship
    const ADMIN_INLINE: bool = true;           // set false to suppress inline
}
```
### 15.7 Opt-Out
```rust
// Exclude a model from the admin entirely
#[derive(Model)]
#[model(table = "internal_tokens", admin = false)]
pub struct InternalToken { ... }

// Hide a specific field (still persisted, not shown)
#[field(admin_hidden)]
pub password_hash: String,
```
### 15.8 Research Areas (Admin)

- Dioxus 0.7 `#[server]` functions sharing the Axum `State<PgPool>` with the main app
- `ForeignKey<T>` select dropdowns for large tables — search-as-you-type rather than loading all options upfront
- Serving the Dioxus WASM bundle as static assets via `tower_http::services::ServeDir` within the Axum router
- Admin session auth — signed cookie, independent of the application's own auth system
- `Jsonb<T>` unknown fields in admin — read-only display with raw JSON view toggle

## 16. Axum Integration

The framework does not hide Axum. Handlers are standard Axum handlers. The pool is accessed via standard `State` extraction.
```rust
async fn vehicle_detail(
    State(pool): State<PgPool>,
    Path(id): Path<HeerId>,
) -> impl IntoResponse {
    match Vehicle::get(&pool, id).await {
        Ok(v)  => Json(v).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
```
Djogi contributes at startup:
- Merges all registered app `Router`s into the root `axum::Router`
- Validates NODE_ID exists in heer_nodes (HeerId startup check) — fails fast if invalid
- Configures standard `tower` middleware (tracing, request ID, optional auth)
- Optionally runs pending migrations on boot (configurable; default on in dev, off in production)

---

## 17. Project Layout
```
my-app/
  src/
    main.rs
    apps/
      vehicles/
        mod.rs              # djogi::register_app!(VehiclesApp)
        models.rs           # #[derive(Model)] structs + ManyToMany impls
        routes.rs           # Axum handlers
      people/
        mod.rs
        models.rs
        routes.rs
  build.rs                  # drift detection + migration generation
  Djogi.toml
  seeds.rhai
  migrations/               # git submodule — managed by pipeline
    schema_snapshot.json
    0001_initial_up.sql
    0001_initial_down.sql
    0002_add_indexes_up.sql
    0002_add_indexes_down.sql

djogi/                      # framework library crate
djogi-macros/               # proc macro crate (separate crate — required by Rust)
djogi-cli/                  # cargo djogi binary
djogi-shell/                # Rhai engine + model bindings
```
---

## 18. Key Research Areas

### Jsonb\<T\> Field Type
- Proc macro generating typed filter accessors for all known fields at every nesting level
- `Jsonb<T>` internal layout: `data: T` + `extra: IndexMap<String, UnknownField>`
- Serde impl: deserialize known fields into `T`, collect remainder into `extra`
- Serialize: merge `data` serialization with `extra` — unknown fields always written back
- Validation: walk the full schema tree before any save, collect all errors with dot-notation paths
- `UnknownField::as_f64()` etc — `Result`-returning, no implicit coercion, clear error variants
- Nested `Jsonb<T>` within `Jsonb<T>` — recursive unknown field handling at each level

### CRUD Logging
- Diffing model fields between pre-save snapshot and current values
- Diffing `Jsonb<T>` fields recursively — walk both `data` and `extra` at each level
- Generating `FieldPath` in dot-notation through arbitrary nesting depth
- `FieldValue::Unknown(UnknownField)` serialization in the JSONB changes column
- Actor attribution: request-context hook design (Axum `Extension` or `State` pattern)
- `CrudLog::objects().json_path_changed("engine.horsepower")` — filtering changes by path in JSONB

### HeeRanjId Integration
- Installing HeeRanjId SQL functions and tables via `cargo djogi init` and `db reset`
- Startup validation: `NODE_ID` env var → `heer_nodes` check → fail fast if missing
- Rust wrappers: `HeerId::generate(&pool)` and `HeerId::generate_many(&pool, n)` calling `generate_id()` / `generate_ids(n)`
- RanjId wrappers: `RanjId::generate(&pool)` calling `generate_ranj_id()`
- `create_with_id()`: explicit ID INSERT with `ON CONFLICT (id) DO NOTHING`
- Shell bindings: synchronous `HeerId::generate()`, `HeerId::generate_many(n)`, and `RanjId::generate()` in Rhai
- HeerId serde impl: `i64` internally, `String` in JSON — no per-field annotation needed
- RanjId serde impl: `Uuid` internally, standard UUID string in JSON
- Migration path: `#[model(pk = "ranjid")]` for models that need higher capacity

### Proc Macro Design
- Struct field injection (`id`, `created_at`, `updated_at`) via `#[derive(Model)]`
- Emitting `ModelDescriptor` via `inventory::submit!` from inside the derive macro
- Side-channel file (`target/djogi_models.json`) for `build.rs` consumption
- Error message quality: `proc-macro-error`, `syn::Error::new_spanned`
- Testing: `trybuild` for compile-fail, `macrotest` for expansion snapshots

### Build.rs Drift Detection
- Reading `target/djogi_models.json` after proc macro expansion
- Timing: ensuring the side-channel file is written before `build.rs` reads it
- Emitting `warning` vs `error` diagnostics from `build.rs`
- Auto-naming generated migrations from detected `SchemaDelta` variants

### QuerySet and Condition Tree
- `AND`/`OR`/`NOT` as a typed `Condition` enum tree
- Translating `Condition` → positional SQL parameters via sqlx `QueryBuilder`
- `prefetch()`: collect PKs from first result, fire one `IN (...)` per relation, stitch back

### ForeignKey Resolved Access
- `ForeignKey<T>` internal layout: `id: i64` + `resolved: Option<Box<T>>`
- Helpful panic message if `.resolved()` called without prior `.prefetch()`

### ManyToMany Trait
- Blanket impl generating named methods from `RELATION` const
- Bidirectional methods from two separate `impl ManyToMany` blocks without collision
- Shell access: Rhai requires concrete types, so each model gets its own registered type

### Shell Transactions
- Holding a single `sqlx::Transaction` across multiple Rhai calls
- Savepoint implementation via `SAVEPOINT name` / `ROLLBACK TO SAVEPOINT name`
- Auto-rollback on shell exit: drop handler or explicit cleanup
- Prompt state indicator for open transactions

### Schema Differ
- Column type equivalence map (Rust type → SQL type → canonical form)
- Rename detection via `#[field(renamed_from)]`
- Destructive operation gating and `--allow-destructive` flag

### Migration Safety
- Postgres advisory locks during migration to prevent concurrent runners
- `--fake` flag: insert into `_sqlx_migrations` without executing SQL
- Checksum verification of already-applied migrations (sqlx handles this natively)

### ConditionBuilder
- Walking the `Condition` enum tree and emitting correct `$n` positional parameters
- `AND`/`OR` grouping with correct SQL parenthesization
- `IN (...)` expansion for `in_list()` with variable-length parameter lists
- JSONB path operator generation for `Jsonb<T>` subfield filters

### Admin Panel (Dioxus)
- Dioxus 0.7 `#[server]` functions sharing Axum `State<PgPool>` with the main app
- `ForeignKey<T>` select dropdowns for large tables — search-as-you-type rather than loading all options upfront
- Serving the Dioxus WASM bundle via `tower_http::services::ServeDir` within the Axum router
- Admin session auth — signed cookie, independent of application auth
- `Jsonb<T>` unknown fields in admin — read-only display with raw JSON view toggle
- Inline pagination and search for large M2M through-tables

### Two-Database Logging Architecture
- Three concurrent connection pool management at startup
- Async write strategy for CRUD log — non-blocking relative to the application request
- `tracing-subscriber` Layer routing spans and events to the event log database
- Severity routing: `WARN`/`ERROR`/`CRITICAL` fanning out to Sentry, OpenTelemetry, Datadog
- `db reset --wipe-crud-logs` / `--wipe-all-logs` guard implementation
---

## 19. Resolved Design Decisions

| Decision | Resolution |
|---|---|
| Default PK type | HeerId — `BIGINT DEFAULT generate_id()`, database-native, time-ordered |
| HeerId ID pattern — default | `create()` uses `DEFAULT generate_id()` + `RETURNING id` |
| HeerId ID pattern — bulk | `HeerId::generate_many(n)` pre-allocates IDs before bulk INSERT |
| HeerId ID pattern — form | `HeerId::generate()` at form render time; INSERT with `ON CONFLICT DO NOTHING` |
| Serial PKs | Opt-in via `#[model(pk = "serial")]` |
| Node identity | NODE_ID environment variable; validated against heer_nodes at startup |
| HeerId JSON serialization | Always string — enforced by HeerId serde impl, no annotation needed |
| Datetime library | `time` crate — cleaner API, no CVE history |
| Framework fields (`id`, `created_at`, `updated_at`) | Macro-injected — real fields, not written by developer |
| `create()` input | Struct itself — no separate `CreateVehicle` wrapper |
| FK loading strategy | Explicit `.fetch()` or `.prefetch()` — no lazy loading |
| M2M strategy | Explicit through models required; `ManyToMany<T>` trait for convenience methods |
| M2M method naming | Explicit `RELATION` const — auto-pluralization is error-prone |
| M2M directionality | Each direction declared separately with its own `impl ManyToMany` |
| Shell async strategy | Dedicated tokio runtime + `block_on` per call — no `.await` in shell |
| Shell transactions | `begin()` / `commit()` / `rollback()` / `savepoint()` — explicit, always rollback on exit |
| Transaction timeout | Optional, developer-chosen at `begin()` — config `transaction_timeout_default` pre-fills prompt (default: 30m) |
| Power loss / crash | Postgres auto-rollbacks dropped connections — no manual recovery needed |
| Shell error handling | Print one-liner + save full traceback to `.djogi_shell_errors/`; session never unwound |
| Shell error log retention | 1 year default; configurable via `error_log_retention` in `Djogi.toml` |
| Shell verbose mode | `cargo djogi shell --verbose` — prints full tracebacks inline in addition to saving to disk |
| Shell history | `.djogi_history` gitignored; `scripts/` committed and shareable |
| Shell import/export | `.export`, `.import`, `.bookmark` — named Rhai scripts in `scripts/` |
| Shell headless execution | `cargo djogi shell --run scripts/name.rhai` — runs script without entering REPL |
| Query engine | Djogi-owned `ConditionBuilder` wrapping `sqlx::QueryBuilder` — no third-party query builder |
| Raw SQLx escape hatch | Always available — `sqlx::QueryBuilder` directly accessible |
| CRUD log architecture | Separate `myapp_crud_logs` database; per-model mirror tables; async writes |
| Event log architecture | Separate `myapp_event_logs` database; `tracing`-powered; severity-routed |
| Log database lifecycle | `db reset` wipes app DB only; `--wipe-crud-logs` / `--wipe-all-logs` for log DBs |
| Admin panel | Optional Dioxus 0.7 fullstack; `admin` feature flag; mounted at `/_admin/` |
| Admin form generation | Auto-generated from `ModelDescriptor` — zero per-model UI code required |
| Admin validation | Auto-generated from field annotations + optional `impl AdminClean` hook |
| Admin M2M inlines | Auto-rendered from `impl ManyToMany<T>`; paginated, searchable, configurable |
| Admin opt-out | Per-model `#[model(admin = false)]`; per-field `#[field(admin_hidden)]` |
| `Jsonb<T>` in admin | Nested fieldset from schema; unknown fields shown read-only with raw JSON toggle |
| Seed script transactions | `seeds.rhai` runs inside a transaction by default — partial failure rolls back entirely |
| Query builder — application code | Closure-based: `.filter(\|f\| f.field.gte(val))` — typed, composable, idiomatic |
| Query builder — shell / dynamic | Programmatic: `ModelFilter::new().field(Gte(val))` — serializable, no closures needed |
| Dirty tracking | Off by default; opt-in globally via `Djogi.toml` or per-model via `#[model(dirty_tracking)]` |
| FK cascade default | `RESTRICT` — safest Postgres default; overridden per-field with `#[field(on_delete = "...")]` |
| Field rename detection | `#[field(renamed_from = "old_name")]` — differ treats as rename not drop+add |
| Build drift diagnostic | Compiler-style `note` (not error) — migration generated, build continues, developer reviews |
| Migration generation | Automatic via `build.rs` on drift detection — generates pair, build continues |
| `makemigrations` CLI | Retained as manual trigger for `--dry-run`, `--allow-destructive`, custom naming |
| Schema snapshot | Updated only on successful `cargo djogi migrate` — reflects actual DB state, never build state |
| Migrations folder | Git submodule — pipeline-managed, invisible to developer day-to-day |
| Migration down files | Always generated as a pair; data loss on destructive rollback documented in file |
| Database target | Postgres only — permanent decision, not a limitation; enables JSONB, HeeRanjId, advisory locks, transactional DDL, `RETURNING` |
| Dev database reset | `cargo djogi db reset` — gated on `dev_mode = true` + localhost URL + `DJOGI_ENV != production` |
| CLI interface | `cargo djogi` subcommand — installed via `cargo install djogi-cli`, idiomatic Rust toolchain |
| Djogi's scope | Model derivation chain only — does not duplicate Axum, SQLx, HeeRanjId, or Tokio responsibilities |
| `Jsonb<T>` field type | `JSONB` column with typed schema, serde deserialization, validator validation, nested schema support |
| Unknown field preservation | Fields not in schema loaded into `extra: IndexMap<String, UnknownField>` — never dropped on save |
| `UnknownField` variants | `String`, `Bool`, `Float`, `Int`, `Null`, `RawJson` — no implicit coercion between types |
| `UnknownFieldError` | `FieldNotFound`, `TypeMismatch`, `NoImplicitCoercion` — all conversions return `Result` |
| Nested `Jsonb<T>` | Fully supported — each nesting level has its own typed schema and unknown field boundary |
| JSONB subfield queries | Typed filter accessors generated per known field using Postgres JSONB path operators |
| CRUD logging | Optional per-model or global; stored in `_djogi_crud_log` table; off by default |
| CRUD log JSON diffing | Dot-notation paths through full `Jsonb<T>` nesting depth including unknown field changes |
| CRUD log actor | Optional `save_with_actor()` or request-context hook; null if not provided |
| Project scaffolding | `cargo djogi new` — scaffolds project and initializes migrations submodule |
