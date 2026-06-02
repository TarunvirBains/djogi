# djogi-macros

Proc macros for the Djogi Model-first Postgres framework — `#[derive(Model)]`, field attributes, and descriptor emission.

**djogi-macros** is the compile-time engine behind Djogi. It provides the procedural macros that transform plain Rust structs into full data-layer models with CRUD operations, typed fields, and schema registration.

## What This Crate Provides

- **`#[derive(Model)]`** — Transforms a struct into a Djogi model with typed CRUD methods, automatic field injection (`id`, `created_at`, `updated_at`), and `ModelDescriptor` emission for migrations and app discovery.
- **Field attributes** — `#[field(...)]` annotations for column configuration: type conversion, indexing, constraints, visage exposure, and schema customization.
- **`#[derive(DjogiEnum)]`** — Generates typesafe Postgres enum codecs for enum fields.
- **`#[derive(JsonbSchema)]`** — Defines the schema for `Jsonb<T>` fields with validation and unknown-field preservation.
- **`djogi::apps!` macro** — Multi-app registration for adopter-side app initialization and per-database model grouping.
- **`djogi_main!` and `link_anchor!` macros** — Adopter-linked CLI wiring to preserve inventory metadata through LTO and prevent silent descriptor drops.

## Important: Adopters Don't Depend on This Crate Directly

You normally depend on **`djogi`**, not `djogi-macros`. The `djogi` crate re-exports all these macros through its prelude:

```rust
use djogi::prelude::*;

#[derive(Model)]
pub struct Vehicle { ... }
```

`djogi-macros` is a separate crate only because Rust requires proc-macro crates to live in their own workspace member. When you `cargo add djogi`, you get access to the macros automatically.

## Targets

PostgreSQL 18 and later.

## See Also

- **[djogi](https://crates.io/crates/djogi)** — The main framework crate. Start here for adopter-side integration and API docs.
- **[Models guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/models.md)** — Deep dive into `#[derive(Model)]`, field annotations, and design patterns.
