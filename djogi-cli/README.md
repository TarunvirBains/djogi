# djogi-cli

CLI for the Djogi Model-first Postgres framework — schema migrations, database management, and descriptor tooling.

**djogi-cli** is the single CLI package for Djogi adopters. It ships **two** installed executables — **`djogi`** (the canonical operator command) and **`cargo-djogi`** (the `cargo djogi` developer wrapper). It handles schema migrations, live-database operations, reference generation, and adopter-linked descriptor integration.

Install both with one command:

```bash
cargo install djogi-cli
```

## Primary Commands

- **`djogi migrations apply`** — Apply pending migrations to the database, with optional `--fake` for existing-schema adoption and `--reason` for metadata.
- **`djogi migrations compose`** — Generate up/down SQL migration pairs from model descriptor drift.
- **`djogi migrations status`** — Show the current state of the ledger, filesystem snapshots, and live database.
- **`djogi migrations verify`** — Validate that the live database matches the expected schema.
- **`djogi migrations repair`** — Reconcile drift between the ledger and live database after interrupted applies.
- **`djogi db reset`** — Drop, recreate, and replay migrations (triple-gated for safety).
- **`djogi db seed`** — Run SQL seeds from `seeds/<database>/*.sql` via the seed ledger.
- **`djogi docs`** — Generate Markdown reference pages from the model descriptor inventory.
- **`djogi schema`** — Dump all registered models as deterministic JSON.

## Descriptor-Free vs. Adopter-Linked

The published `djogi` binary on crates.io runs **descriptor-free commands** — those that work on pre-composed migration artifacts. Commands like `apply`, `status`, and `seed` run this way.

For descriptor-dependent commands during **local development**, invoke the
adopter-linked binary through the `cargo djogi` wrapper (the `cargo-djogi`
executable that `cargo install djogi-cli` installed):

```bash
cargo djogi <command...>
```

with these `Djogi.toml` keys configured:

```toml
[cli]
package = "my-adopter-app-bin"
bin = "djogi"
```

> `cargo djogi` is a **local-development convenience** — it shells out to
> Cargo to build and forward to your adopter-linked `djogi` binary. `djogi`
> is the canonical operator command. **Production and CI run the prebuilt
> adopter-linked `djogi` binary directly** (for example inside a migration
> image), never `cargo djogi`, because the wrapper requires a buildable
> workspace and a Cargo toolchain.

Descriptor-dependent commands (`compose`, `verify`, `schema`, `docs`) require an **adopter-linked CLI** built in your own workspace. Wire it with one dependency line:

```toml
[dependencies]
djogi = "0.1.0-alpha.7"
djogi-cli = "0.1.0-alpha.7"
```

And a tiny binary entry point named `djogi` (`src/bin/djogi.rs`). The `djogi_main!` macro *generates* a `main` that references one model type per linked crate — forcing the linker to retain each crate's model inventory — then runs the CLI:

```rust
// List at least one #[derive(Model)] type from every model crate.
djogi_cli::djogi_main!(my_models::Vehicle, my_models::Person);
```

Or write the entry point by hand:

```rust
fn main() -> std::process::ExitCode {
    // Force-link the model crate so its descriptors survive linking.
    let _ = <my_models::Vehicle as djogi::model::Model>::descriptor();
    djogi_cli::run_from_env()
}
```

## Exit Codes

- **0** — Command succeeded.
- **1** — Runtime error (e.g., database unavailable, migration failed).
- **2** — Refusal (e.g., `--fake` rejected due to data drift, `reset` cancelled by user).

## Targets

PostgreSQL 18 and later.

## See Also

- **[djogi](https://crates.io/crates/djogi)** — The main framework crate.
- **[Migrations guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/migrations.md)** — Schema evolution, drift detection, and recovery workflows.
- **[Adopter CLI guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/adopter-cli.md)** — How to build and wire the adopter-linked CLI in your workspace.
- The previously-published standalone **`cargo-djogi`** crate is superseded — both executables now ship from `djogi-cli`. Install `djogi-cli`; there is no separate `cargo-djogi` package.
