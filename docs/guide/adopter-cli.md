> [Back to README](../../README.md) | [All Guides](./index.md)

# Adopter-Linked `djogi` CLI

The published `djogi` binary links the framework crates only — it sees no adopter `#[model]` structs. To run descriptor-dependent commands (`migrations compose`, `schema`, `docs`) against your models, you build an **adopter-linked** `djogi` binary that references your model crates at link time.

This guide walks through setting up that binary, the three invocation models (dev, CI, production), and troubleshooting common failures.

---

## 1. Why Adopter-Linked?

Djogi discovers models via Rust's `inventory` registry, which is a **link-time** mechanism: a binary only sees descriptors from crates linked into it. The standalone published binary links `djogi` + `djogi-cli` and **no adopter model crates**, so it sees zero models.

The solution is minimal: add one dependency line and a tiny binary file to your workspace, then invoke `cargo djogi` instead of the published binary for descriptor-dependent commands.

---

## 2. Minimal Adopter Setup

### Add Dependencies

```toml
[dependencies]
djogi = "0.1"
djogi-cli = "0.1"
```

### Create the Binary File

**Option A — Hand-written (recommended for understanding):**

```rust
// src/bin/djogi.rs
fn main() -> std::process::ExitCode {
 // Force link-time registration of every crate that defines #[model] structs.
 // You must reference at least one model type from each model crate, or the
 // linker may drop that crate's inventory registrations entirely.
 adopter_models::link_models();
 djogi_cli::run_from_env()
}
```

The `link_models()` function is a thin helper you define in your model crate:

```rust
// In each model crate's lib.rs (or a dedicated module):
pub fn link_models() {
 // Reference at least one model type per crate to force linkage.
 // The exact symbol referenced does not matter — it only needs to prevent
 // the linker from dropping the crate's inventory statics.
 // Call descriptor() (note the parentheses): a bare `::descriptor` fn-item
 // reference can be optimized away, whereas the call forces the symbol —
 // the same primitive djogi_main! and sync_models use.
 let _ = <YourModel as djogi::model::Model>::descriptor();
}
```

> **You must reference every crate that defines `#[model]` structs.** If a model crate is not referenced, its models are invisible to `compose`, `schema`, and `docs`. See [Troubleshooting](#4-troubleshooting) for the diagnostic message.

**Option B — Macro-generated (recommended for adoption):**

```rust
// src/bin/djogi.rs
djogi_cli::djogi_main!(adopter_models::Elephant, billing::Invoice);
```

The `djogi_main!` macro expands to a `main` function that:
1. References each listed type's `<T as Model>::descriptor()` (calling it forces link-time registration of that type's crate)
2. Calls `djogi_cli::run_from_env()`

> **You must list at least one model type from every crate that defines `#[model]` structs.** The macro cannot discover your workspace's crate graph — exhaustiveness is your responsibility. If you have models in three crates, list at least one type from each. An empty `djogi_main!()` (no types) is a compile error.

**Option C — Link anchor (for large workspaces):**

If maintaining the model type list is burdensome, place a `link_anchor!` in each model crate's `lib.rs`. It takes no arguments — it is a per-crate marker that emits one `__djogi_link_anchor()` symbol per crate:

```rust
// In each model crate's lib.rs, once.
// A model crate depends on `djogi` (not `djogi-cli`), and `link_anchor!`
// is re-exported from `djogi`, so reach it through the `djogi` path here.
djogi::link_anchor!();
```

Then write a hand-written `fn main()` that references each model crate's anchor once and calls `djogi_cli::run_from_env()`:

```rust
// src/bin/djogi.rs
fn main() -> std::process::ExitCode {
 // One reference per model crate. Referencing the anchor pulls the
 // crate's rlib member into the binary, so its #[derive(Model)]
 // inventory statics survive --gc-sections / LTO.
 tracker::__djogi_link_anchor();
 billing::__djogi_link_anchor();
 djogi_cli::run_from_env()
}
```

The anchor is `#[doc(hidden)]` and carries the `#[used]` attribute that prevents the crate's inventory statics from being dead-stripped. As with `link_models` / `djogi_main!`, you must reference **every** crate that defines `#[model]` structs — the anchor only removes the per-model type list, not the per-crate exhaustiveness requirement.

### Workspace Layout Example

```
myapp/
├── Cargo.toml   # workspace manifest
├── Djogi.toml   # app configuration
├── models/
│ ├── Cargo.toml
│ └── src/
│ ├── lib.rs  # defines #[model] structs + link_models()
│ └── elephant.rs  # Elephant model
├── billing/
│ ├── Cargo.toml
│ └── src/
│ ├── lib.rs  # defines #[model] structs + link_models()
│ └── invoice.rs  # Invoice model
└── bin/
 ├── Cargo.toml  # depends on models, billing, djogi-cli
 └── src/
 └── bin/
  └── djogi.rs  # the adopter-linked binary (4 lines)
```

---

## 3. Invocation Models

### Development: `cargo djogi`

Run descriptor-dependent commands from your workspace root:

```toml
[cli]
package = "my-adopter-app-bin" # package name that defines your `src/bin/djogi.rs`
bin = "djogi"   # executable built by that package
```

```bash
# Compose migrations from model drift
cargo djogi migrations compose

# Verify live schema against snapshot
cargo djogi migrations verify

# Render schema documentation
cargo djogi schema

# Generate model documentation
cargo djogi docs
```

This works identically to the published `djogi` binary, but your binary links your model crates so descriptor discovery succeeds.

### CI / Container: Prebuilt Binary

Build the adopter-linked binary once and copy it to a minimal runtime environment:

```dockerfile
# Build stage — compiles the adopter-linked binary with all model crates
FROM rust:1.95 AS builder
WORKDIR /app
COPY..
RUN cargo build --bin djogi --release

# Runtime stage — only the binary, config, and artifacts needed
FROM debian:bookworm-slim
COPY --from=builder /app/target/release/djogi /usr/local/bin/
COPY Djogi.toml /app/Djogi.toml
COPY target/djogi_pending /app/target/djogi_pending
WORKDIR /app

# No Cargo, Rust toolchain, or source needed at runtime
ENV DATABASE_URL=${DATABASE_URL}
ENV HEER_NODE_ID=7
CMD ["djogi", "migrations", "apply"]
```

The binary contains all model descriptors baked in via link-time inventory. At runtime, it only needs:
- The compiled `djogi` binary
- `Djogi.toml` configuration
- Pending artifacts (for `apply`) or snapshot files (for `verify`)
- `DATABASE_URL` environment variable
- Node identity for apply (`--node-id`, `HEER_NODE_ID`, or `--single-node-dev`)

### Production Apply: Standalone Binary

The **published standalone** `djogi` binary can still run `migrations apply` against already-composed pending artifacts. This is useful when you compose migrations in CI (using the adopter-linked binary) and later ship only the pending artifacts + standalone binary. This path is descriptor-free, not identity-free: `apply` still requires `--node-id <id>`, `HEER_NODE_ID`, or `--single-node-dev`.

```bash
# In CI — compose with adopter-linked binary
cargo djogi migrations compose

# On a fresh local/ephemeral database — apply with standalone binary
djogi migrations apply --single-node-dev

# In a registered cluster/prod environment — bind an existing node identity
HEER_NODE_ID=7 djogi migrations apply
```

Selected-node apply (`--node-id` or `HEER_NODE_ID`) binds an already-registered cluster node. On a virgin database, use `--single-node-dev` for local provisioning; production profile refuses that mode.

`apply` reads pre-composed JSON from `target/djogi_pending/<db>/<app>.json` and, for auto-emitted Phase 0, `target/djogi_pending/<db>/.phase_zero/<version>.json`. Ship the entire `target/djogi_pending` tree, including the hidden `.phase_zero` subtree. No descriptor discovery is required at apply time.

---

## 4. Troubleshooting

### "no djogi models are registered in this binary"

This is the exact first line the CLI prints (`error: no djogi models are registered in this binary (djogi <command>).`) when a descriptor-dependent command (`compose`, `verify`, `schema`, or `docs`) resolves zero model descriptors. The full message names both causes below. Causes:

**You ran the standalone published `djogi` binary.** That binary links no application models. Build an adopter-linked binary (see [Minimal Adopter Setup](#2-minimal-adopter-setup)) and run the command from it. The standalone binary can still run `djogi migrations apply` against already-composed artifacts, but it still needs node identity (`--node-id`, `HEER_NODE_ID`, or `--single-node-dev`), and selected-node apply expects an already-registered cluster node.

**You ran your adopter-linked binary but forgot to link a model crate.** Ensure `link_models()` references every crate defining `#[model]` structs, or that `djogi_main!` lists at least one type from each model crate. If a crate is not referenced by any symbol in the binary, the linker may drop its `inventory` statics entirely.

**Partial linkage (dangerous):** If some crates are linked and others are not, `compose` sees only the linked models. The **linkage-aware drop guard** prevents silent data loss: if an app was previously registered (in `schema_snapshot.json`) but now has zero projected models, `compose` refuses with a targeted diagnostic — even when `--allow-destructive` is set.

### Compose Refuses with "app X was previously registered"

The linkage-aware drop guard detected that an app had models in the last snapshot but has zero models in the current projection. This typically means:
- A model crate was forgotten in `link_models()` or `djogi_main!`
- An intentional app removal needs to use the `tombstone` mechanism instead

To intentionally remove an entire app's tables, mark the app as a tombstone in your configuration rather than unlinking its crate.

### Verify Degrades Silently

Unlike other commands, `verify` degrades gracefully when no descriptors are present but on-disk snapshots exist: it enumerates buckets from the snapshot files and verifies the live database against them. This is a deliberate standalone capability — checking a deployed database against the shipped snapshot requires no model crates.

If you see no verification output and no errors, check whether your adopter-linked binary is actually linking model crates (see [linkage troubleshooting](#no-djogi-models-are-registered-in-this-binary)).
