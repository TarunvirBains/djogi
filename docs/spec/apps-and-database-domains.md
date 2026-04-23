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

Apps are declared explicitly:

```rust
djogi::apps! {
    Vehicles,
    Users,
    Orders,
}
```

The macro defines a sealed set of app/domain identifiers for the crate.

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
    Vehicles,

    #[app(database = "main")]
    Users,

    #[app(database = "crud_log")]
    Audit,
}
```

The default database target is `main`.

Validation contract:

- every model inherits the database target of its app/domain
- models without `app = ...` belong to the default bucket and target `main`
- the macro rejects `database = ...` on `#[model(...)]` with an error that points at the correct place for the declaration (the enclosing `djogi::apps!` block)

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

Recommended on-disk layout:

```text
migrations/
├── main/
│   ├── schema_snapshot.json
│   ├── 0001_initial_up.sql
│   ├── 0001_initial_down.sql
│   └── ...
├── crud_log/
│   ├── schema_snapshot.json
│   └── ...
└── event_log/
    ├── schema_snapshot.json
    └── ...
```

If app/domain grouping is enabled within a target, that grouping nests inside the target boundary rather than replacing it.

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
