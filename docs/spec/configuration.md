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

This runtime registration surface is separate from the schema-ownership domain surface described in [Apps & Database Domains](./apps-and-database-domains.md):

- `djogi::register_app!` = runtime registration of models/routes/descriptors
- `djogi::apps!` = compile-time schema ownership domains and database-target grouping

They are complementary, not competing APIs.
---

## 12. Configuration

`Djogi.toml` at the project root:
```toml
[database]
url = "postgres://localhost/myapp"
crud_log_url = "postgres://localhost/myapp_crud_logs"
event_log_url = "postgres://localhost/myapp_event_logs"
max_connections = 10
dev_mode = false
# NODE_ID is set as an environment variable, not in Djogi.toml — it is infrastructure config

[logging]
profile = "balanced"      # one of: light, balanced, strict_audit

# Optional escape hatches for teams with unusual requirements.
# Normal adopters should pick a profile and stop there.
crud_delivery = "derive"  # derive | best_effort | durable | fail_closed
event_delivery = "derive" # derive | off | best_effort | durable

[server]
host = "0.0.0.0"
port = 8000

[migrations]
submodule = "migrations"
allow_destructive = false
lock_timeout_secs = 30

[shell]
history_file = ".djogi_history"                # gitignored — personal and noisy
transaction_timeout_default = "30m"            # pre-fills the begin() prompt; developer can clear it
scripts_dir = "scripts"                        # committed, shareable shell scripts
error_log_dir = ".djogi_shell_errors"          # gitignored — full tracebacks on disk
error_log_retention = "1y"                    # auto-purge logs older than this on shell startup

[features]
dirty_tracking = false
```
`DATABASE_URL` env var always overrides `[database].url`. `CRUD_LOG_URL` and `EVENT_LOG_URL` likewise override their matching `[database]` entries when set. Secrets live in env vars, never in `Djogi.toml`.

Logging should be easy to adopt. The intended maintainer workflow is to choose a `logging.profile` and use the defaults:

- `light` — app database required; CRUD and event logs are best-effort and may be dropped during sink outages
- `balanced` — app database required; CRUD logs use durable bounded retry with health warnings; event logs remain best-effort
- `strict_audit` — app database required; CRUD logs are fail-closed; event logs remain best-effort unless explicitly overridden

The explicit `crud_delivery` and `event_delivery` keys are escape hatches, not the primary UX. Djogi should document profile-based setup first and treat individual overrides as advanced operations work.
---

## 14. CLI — `cargo djogi`

Installed once, used everywhere:
```bash
cargo install djogi-cli
```
```bash
# Migrations
djogi migrations compose               # generate migration files from current drift
djogi migrations compose --dry-run     # preview SQL without writing files
djogi migrations compose --allow-destructive
djogi migrations apply                 # apply pending migrations, update snapshot
djogi migrations apply --fake 0005     # mark applied without running SQL
djogi migrations rollback              # roll back last migration, rewind snapshot
djogi migrations status                # show file/ledger/snapshot state
djogi migrations verify                # compare snapshot expectations to the live DB
djogi migrations repair                # resolve partial apply or rebuild snapshot state
djogi migrations baseline 0001_initial # adopt an existing DB without replaying SQL

# Migration-history state management
djogi migrations attune                # attune local migration-history files to the repo-default target
djogi migrations attune <target>       # attune to a local or remote git target
djogi migrations attune <target> --verify
djogi migrations attune <target> --record
djogi migrations attune --squash       # dev-only local squash of migration history
djogi migrations attune --squash --push

# Database (dev only — gated on dev_mode + localhost + DJOGI_ENV != production)
djogi db reset                         # drop → recreate → migrate
djogi db reset --seed                  # drop → recreate → migrate → seed
djogi db seed                          # run seeds.rhai only

# Shell
djogi shell

# Project scaffolding
djogi new my-project                   # scaffold project + init migrations submodule
djogi init                             # add Djogi to existing project
```
`db reset` hard-errors unless all three guards pass: `dev_mode = true`, `DATABASE_URL` resolves to localhost, `DJOGI_ENV` is not `production`.

`migrations attune` manages local migration-history Git state. It may fetch remote refs when needed to resolve a target, but it does not mutate the database unless `--apply` is explicitly passed. Parent-repo submodule-pointer changes are explicit via `--record` or options that clearly imply recording, such as `--squash`.

`migrations attune` target contract:

- target may be omitted, in which case Djogi attunes to the repo-default/expected migration-history state
- target may be a local or remote commit, tag, or branch
- if `migrations/` has no remote configured, attune is limited to locally available Git targets
- `--record` updates the parent repo's recorded submodule pointer after successful attunement
- `--squash` is hard-gated exactly like `db reset`: `dev_mode = true`, localhost URL resolution, and `DJOGI_ENV != production`
- `--squash` should refuse when the migration history is already treated as shared staging/production history
- `--squash --push` requires a configured remote
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
