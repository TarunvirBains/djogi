> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# Configuration, CLI & Integration

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
Apps are registered at link time via `inventory`. At startup Djogi collects all registered apps, merges their `Router`s when a web-framework feature flag is active (for example `axum`), and makes all `ModelDescriptor`s available to the differ and shell. With no web-framework flag enabled, Djogi still registers app models and descriptors — only the router-merge step is skipped, letting adopters wire HTTP manually.
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

## 16. Web Framework Integration

Djogi is framework-agnostic at the core. HTTP routing, middleware, and extraction belong to whichever Rust web framework the adopter chooses, enabled through a single per-framework feature flag (one flag per framework, never per-feature×framework sub-flags). Today `axum` is the best-covered option; future flags (`warp`, `actix`, etc.) follow the same pattern.

The rest of this section uses Axum as a concrete example. Enable it with:

```toml
djogi = { version = "0.1", features = ["axum"] }
```

Djogi does not wrap Axum in a second routing abstraction. Handlers stay ordinary Axum handlers, and the pool is accessed through standard `State` extraction.
```rust
async fn vehicle_detail(
    State(pool): State<PgPool>,
    Path(id): Path<HeerId>,
) -> impl IntoResponse {
    let mut ctx = DjogiContext::from_pool(pool.clone());
    match Vehicle::get(&mut ctx, id).await {
        Ok(v)  => Json(v).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
```
Djogi contributes at startup (under the `axum` feature):
- Merges all registered app `Router`s into the root `axum::Router`
- Validates NODE_ID exists in heer_nodes (HeerId startup check) — fails fast if invalid
- Configures standard `tower` middleware (tracing, request ID, optional auth)
- Optionally runs pending migrations on boot (configurable; default on in dev, off in production)

The NODE_ID validation and migration-on-boot behaviours are framework-agnostic — they run whether or not a web-framework flag is enabled. Only the router-merging glue is Axum-specific here; equivalent glue ships under each future framework flag.
