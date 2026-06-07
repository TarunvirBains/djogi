> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# Apps & Database Domains

Djogi supports an explicit apps/domain layer so a modular monolith can be organized cleanly today and split into service-owned boundaries later.

This spec defines:

- app/domain registration for schema ownership
- database-target subscription for models and apps
- migration grouping by app/domain and database target
- relation boundaries across database targets

It does **not** define cross-service runtime orchestration or distributed transactions.

---

## Goals

- make schema ownership explicit
- support multiple database targets without hiding the target boundary
- keep migration history groupable by app/domain
- allow future service extraction without rewriting the conceptual model
- reject relation patterns Djogi cannot safely guarantee

---

## Core Model

Djogi distinguishes two axes:

- **app/domain** — organizational schema ownership
- **database target** — the physical database a model belongs to

Examples of database targets:

- `main`
- `crud_log`
- `event_log`
- future service-owned targets such as `billing` or `analytics`

An app/domain may subscribe to exactly one primary database target.

That rule is deliberate:

- it keeps migration planning target-local
- it keeps foreign-key guarantees honest
- it maps cleanly to later service extraction

For the public contract, database targets are named string identifiers with a small built-in default set:

- `main`
- `crud_log`
- `event_log`

Future service-owned targets may be introduced as additional string identifiers such as `billing` or `analytics`.

The identifier grammar should stay boring:

- ASCII lowercase letters
- digits
- underscore

Examples:

- `main`
- `crud_log`
- `event_log`
- `billing`

---

## App Declaration

Apps are declared explicitly with a required database target:

```rust
djogi::apps! {
    #[app(database = "main")]
    pub struct Vehicles;

    #[app(database = "main")]
    pub struct Users;

    #[app(database = "main")]
    pub struct Orders;
}
```

The macro emits each entry as a zero-sized unit struct bound to a sealed `djogi::App` trait; apps are addressed by **type path**, not by string label. Rust's own name resolution enforces declaration — `#[model(app = Vehicles)]` referring to an undeclared or non-app type fails with a standard rustc error. (Phase 7-Zero v3 §4B, Codex P0-03 fix 2026-04-23.)

`database = "..."` is required per app declaration in T7. There is no implicit default — an app without an explicit target is a compile error so tables never silently land in the wrong database. Models that don't opt into an app fall into the synthetic global bucket which targets `main` by default.

### Sealing model — convention at compile time, verified at migration time

The `djogi::App` trait is **convention-sealed**, not hard-sealed. A determined downstream crate that reaches into `#[doc(hidden)] pub` items (`djogi::apps::SealToken`, `djogi::apps::__DJOGI_APPS_SEAL_TOKEN`) can hand-write an `impl djogi::App for MyFake` that compiles. True hard-sealing of a trait whose implementations are emitted by a proc macro is **not achievable in stable Rust** when that proc macro lives in a separate crate (as proc macros must) — every pub path the macro reaches from downstream context is also reachable by handwritten downstream code.

The correctness invariant that actually matters is **not** "downstream cannot construct an `App` impl" but **"a forged `App` impl cannot silently break migrations."** That invariant is enforced at the use site, by Phase 7's migration differ: every `#[model(app = X)]` is cross-checked against `AppRegistry::all()` at migration startup, and any model pointing at an App-implementing type whose `AppDescriptor` is missing from inventory hard-errors before any SQL executes. Forged App impls are legal Rust but inert — they never reach the migration path.

Ecosystem precedent (serde, tokio, axum) confirms this convention: user-facing traits implemented via proc macros in separate crates are convention-sealed + use-site-verified, never hard-sealed.

Models opt in explicitly:

```rust
#[model(app = Vehicles)]
pub struct Vehicle {
    pub make: String,
}
```

Models without `#[model(app = ...)]` belong to the global/default bucket.

This apps/domain declaration is distinct from the older runtime app-registration surface used for routes and descriptor discovery.

The intended reconciliation is:

- `djogi::apps!` declares compile-time schema ownership domains
- `djogi::register_app!` registers runtime app modules/routes/descriptors

They may often use the same human names, but they solve different problems and should not be conflated.

---

## Database Target Subscription

Apps declare which database target they belong to. Models inherit their app's database target; there is no model-level `database = ...` attribute. This matches the rule stated in [models.md](./models.md) §4.1.

Representative shape:

```rust
djogi::apps! {
    #[app(database = "main")]
    pub struct Vehicles;

    #[app(database = "main")]
    pub struct Users;

    #[app(database = "crud_log")]
    pub struct Audit;
}
```

The synthetic global bucket — the destination for models that never opt into an app — targets `main`. Named apps have no implicit database default; every `#[app(...)]` must carry an explicit `database = "..."` in T7.

Validation contract:

- every model inherits the database target of its app/domain
- models without `app = ...` belong to the synthetic global bucket and target `main`
- named apps without `#[app(database = "...")]` are a compile error
- the macro rejects `database = ...` on `#[model(...)]` with an error that points at the correct place for the declaration (the enclosing `djogi::apps!` block)

---

## Lifecycle Markers (T8)

Apps carry optional markers for the retirement flow:

- `#[app(renamed_from = "old_label")]` — declares the app is the continuation of a prior label. Phase 7's differ generates an `ALTER SCHEMA ... RENAME`.
- `#[app(tombstone)]` — flags the app for retirement in this compose cycle. Phase 7 gates destructive migration generation behind `--allow-destructive`.
- `#[model(moved_from_app = OldBilling)]` — historical-metadata pointer on a model whose prior app is being retired.

`tombstone` and `renamed_from` are mutually exclusive within one `#[app(...)]`. `#[model(app = X)]` on a tombstoned app is a compile error — active models must either stay on a live app or use `moved_from_app = X` instead. For the full two-cycle retirement flow with concrete snippets, see [`docs/guide/apps.md`](../guide/apps.md).

---

## Cross-App FK Graph (T9)

`AppRegistry::cross_app_edges()` returns every FK edge where source and target apps differ (as `(database, label)` identities). `AppRegistry::cross_app_cycles()` returns cross-app cycles as `Vec<AppIdentity>` paths. Phase 7's differ uses both:

- Ordering: per-app compose steps apply in target-before-source order so FKs resolve at DDL time.
- Cycle rejection: cross-app cycles become a migration-time error with the cycle path.
- Same-database required: cross-database FKs are structurally impossible (Postgres cannot enforce them); the differ rejects these at compose time.

Both `OneToOne` and `ForeignKey` relation kinds count as edges. Intra-app FKs are omitted — source and target share a compose boundary and are always safe.

`AppDiagnostic` carries Phase 7 D004 (folder drift — directory on disk without descriptor) and D010 (ledger has an `app_label` with no descriptor match). Detection logic lives in Phase 7 proper.

**Current limitation — short-name type lookup.** The graph resolves `ForeignKey<T>` / `OneToOneField<T>` targets by looking up `T`'s short Rust identifier (e.g. `"User"`) in `inventory::iter::<ModelDescriptor>`. Two distinct models with the same short name across different modules or crates in the same workspace would collide in the lookup — whichever inserts last wins, and edges can route to the wrong app. The working convention is that model type names are unique across the linked crate graph. A future descriptor-shape change will key lookups on `(module_path, type_name)` to remove the limitation; until then, workspace model names must be globally unique.

---

## Migration Grouping

If the apps/database-domains subsystem is enabled, migrations are grouped by:

- `database_target`
- `app_label`

That means:

- each database target has its own ledger
- each database target has its own snapshot set
- app/domain grouping happens within that target boundary

The migration engine still applies one database target at a time. Djogi does not pretend a single migration run across `main`, `crud_log`, and `event_log` is one distributed transaction.

On-disk layout — snapshots and migration files nest per-(target, app):

```text
migrations/
├── main/
│   ├── vehicles/
│   │   ├── schema_snapshot.json
│   │   ├── V20260425010203__initial.sdjql
│   │   ├── V20260425010203__initial.down.sdjql
│   │   └── ...
│   ├── users/
│   │   ├── schema_snapshot.json
│   │   └── ...
│   └── ...
├── crud_log/
│   └── audit/
│       ├── schema_snapshot.json
│       └── ...
└── event_log/
    └── ...
```

Granularity differs by artifact:

- **Per `(target, app)` pair:** directory, snapshot (`migrations/<target>/<app>/schema_snapshot.json`), pending build-artifact (`target/djogi_pending/<target>/<app>.json`) for normal buckets, the migration SQL files within each app directory (Phase 7 v3 OQ-10 ruling 2026-04-23), and the advisory-lock namespace — keys are derived from `SHA-256("djogi:advisory_lock:" || database || "\0" || app)` (first 8 digest bytes as a big-endian signed 64-bit integer). Independent `(database, app)` buckets hash to distinct keys, so apps within the same target do not contend on a shared lock. Auto-emitted Phase 0 uses a separate hidden pending namespace at `target/djogi_pending/<target>/.phase_zero/<version>.json` so it can coexist with normal global pending; build diagnostics preserve that hidden path instead of reporting it as `_global_.json`. (See `docs/spec/decisions.md` row "Migration advisory lock key".)
- **Per `target`:** the `djogi_schema_migrations` ledger table — one per database target, shared across all apps in that target. (See `docs/spec/decisions.md` row "Multi-database migration contract.")

Each `(database, app)` bucket is serialized by its own advisory-lock key, and cross-target migrations run independently with their own locks; independent buckets within one target do not block one another.

---

## Relation Boundary

Foreign keys are same-target only.

Hard rule:

- a `ForeignKey<T>` is valid only when source model and target model resolve to the same `DatabaseTarget`
- a many-to-many through model is valid only when all participating models resolve to the same `DatabaseTarget`

Cross-database foreign keys are explicitly rejected.

Reason:

- Djogi cannot provide a real database-enforced FK across separate databases
- pretending otherwise would make the ORM contract dishonest
- it conflicts with the modular-monolith to microservice path

Allowed alternative for cross-database references:

- store the referenced ID as a scalar field
- optionally store denormalized identifying fields
- enforce consistency in application/workflow logic

This is an application-level reference, not an ORM foreign key.

Enforcement contract:

- descriptor validation must reject cross-target `ForeignKey<T>`
- descriptor validation must reject cross-target M2M through models
- errors should name both source and target models and the conflicting database targets

---

## Lifecycle Operations

Apps/domains may undergo lifecycle transitions similar to models and fields:

- add
- rename
- retire/tombstone
- move model between apps/domains

These lifecycle markers exist to support clean migration history and organizational changes. They do not weaken the database-target boundary.

A model move between apps/domains is allowed only when:

- the source and target apps share the same database target, or
- the move is expressed as an explicit cross-target migration/replacement plan rather than a no-op organizational move

Moving a model across database targets is therefore not a rename-style metadata event. It is a data-placement change and must be treated as a higher-risk migration workflow.

---

## Service-Extraction Path

This subsystem exists partly to support future splitting of a modular monolith into service-owned pieces.

The intended path is:

1. start with app/domain ownership inside one codebase
2. make database-target boundaries explicit
3. forbid fake cross-database referential guarantees
4. later move an app/domain from `main` to a service-owned target with an explicit migration plan

Djogi should make that path possible without promising that the transition is trivial or automatic.

---

## Boundary

Djogi owns:

- app/domain metadata
- database-target metadata
- migration grouping by target and app/domain
- same-target relation validation
- explicit rejection of cross-database foreign keys

Djogi does not own:

- distributed transactions across targets
- cross-service consistency protocols
- service-mesh routing
- application-specific saga/workflow logic
