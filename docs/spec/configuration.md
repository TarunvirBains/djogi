> [Back to README](../../README.md) | [All Specs](./index.md)

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
# HEER_NODE_ID and HEER_RANJ_NODE_ID are set as environment variables, not in Djogi.toml — they are infrastructure config

[logging]
profile = "balanced"      # one of: light, balanced, strict_audit

# Optional escape hatches for teams with unusual requirements.
# Normal adopters should pick a profile and stop there.
crud_delivery = "derive"  # derive | best_effort | durable | fail_closed
event_delivery = "derive" # derive | off | best_effort | durable

[server]
host = "0.0.0.0"
port = 8000

[cli]
# Optional in production binaries; required for `cargo djogi` in adopters.
# `package` is the workspace package name that contains your adopter-linked `djogi` binary.
# `bin` is that binary's executable name.
package = "my-app-bin"
bin = "djogi"

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

## 14. CLI — `djogi`

### CLI Wrapper Discovery (`cargo djogi`)

- `[cli].package` must be set in `Djogi.toml` for `cargo djogi` to locate and build the adopter-linked binary.
- `[cli].bin` defaults to `djogi` when omitted.
- `cargo djogi` loads this file from the workspace root, builds the configured package/bin with Cargo, then executes the resulting binary with your CLI args.

The `cargo djogi` invocation is provided by the `cargo-djogi` executable,
which ships from the **`djogi-cli`** package — not a separate `cargo-djogi`
crate. Installing `djogi-cli` installs both the `djogi` and `cargo-djogi`
executables. `cargo djogi` is a local-development wrapper: production and CI
must run the prebuilt adopter-linked `djogi` binary directly, because the
wrapper shells out to Cargo and requires a buildable workspace.

Installed once, used everywhere:
```bash
cargo install djogi-cli
```
```bash
# Migrations — drift composition and state
djogi migrations compose               # generate migration files from current drift
djogi migrations compose --allow-destructive
djogi migrations status                # show file/ledger/snapshot state

# Migration-history state management
djogi migrations attune                                    # diff-only ledger / disk reconciliation (read-only)
djogi migrations attune <target>                           # resolve target (Git commit / tag / branch); diff-only without --apply
djogi migrations attune <target> --apply                   # diff + commit ledger / disk mutations
djogi migrations attune <target> --apply --record          # also update parent repo's recorded submodule pointer
djogi migrations attune --record-ledger --apply            # insert ledger rows for unrecorded SQL files
djogi migrations attune --squash --from V<ts> --apply      # dev-only local squash of migration history
djogi migrations attune --squash --from V<ts> --apply --publish   # squash and push the rewritten submodule

# Migrations — shipped CLI (library APIs available for all)
# The library entry points (`apply_plan`, `rollback_plan`, `repair_*`,
# `baseline_plan`) are public and exercised by the integration test suite.
# `apply`, `verify`, `repair`, `baseline`, and `rollback` ship as CLI commands.
djogi migrations apply                 # apply pending migrations, update snapshot
djogi migrations apply --fake --reason "existing schema"  # mark applied without running SQL
djogi migrations verify                # compare snapshot expectations to the live DB
djogi migrations verify --strict       # promote out-of-order diagnostics to errors
djogi migrations repair checksum-drift V<ts>__<slug> --checksum-up V1:<hex>  # re-checksum an edited applied row
djogi migrations repair partial-apply V<ts>__<slug> rolled-back  # resolve a partial-apply ledger row
djogi migrations repair resume-partial V<ts>__<slug>  # resume an interrupted non-transactional apply
djogi migrations repair snapshot-rebuild --app <label>  # rebuild a bucket snapshot from ledger + live DB
djogi migrations baseline V<ts>__baseline --reason "existing schema"  # adopt an existing DB without replaying SQL
djogi migrations rollback --single-node-dev  # roll back newest migration, re-project snapshot

# Database (dev only — triple-gated)
djogi db reset                         # drop → recreate → replay; refuses without --yes / interactive y
djogi db reset --yes                   # non-interactive — typical for CI
djogi db reset --yes --allow-checksum-drift-reset # explicit override when ledger/file parity drift is known and accepted
djogi db seed                          # run seeds/<database>/*.sql files; idempotent via djogi_seed_runs ledger
djogi db seed --database crud_log      # operator-supplied database — splices into URL path for routing
djogi db seed --allow-non-localhost    # opt in to remote DBs (CI integration suites)

# Documentation
djogi docs                             # render Markdown reference pages from descriptor inventory

# Shell — Phase 9 (deferred)
# djogi shell  # target command is planned for Phase 9; not registered in v0.1.0 CLI

# Project scaffolding — Phase 7+ follow-up
djogi new my-project                   # scaffold project + init migrations submodule
djogi init                             # add Djogi to existing project
```

### CLI Exit-Code Matrix

Every `db` and `migrations` subcommand obeys a uniform three-value exit-code
matrix so shell integrations can distinguish "operation refused" from
"operation failed":

| Code | Meaning |
|------|---------|
| `0`  | Success — the command completed and any post-state was applied. |
| `1`  | Error — config load failure, network, SQL, replay, or any other underlying runtime failure. |
| `2`  | Refusal — either a policy gate (localhost, production profile, missing `--yes`, …) blocked execution before any side effect, OR clap-style argument validation rejected the invocation (missing flag, mutually exclusive flags). |

Exit code `2` deliberately bundles policy refusals and
argument-validation errors. Clap's default behaviour is to return `2`
for unknown / malformed flags; manual `2` returns in
`migrations attune` (missing `--from`, conflicting flags) and the
`db reset` / `db seed` gates intentionally share that code so a CI
script can treat any `2` as "operator must intervene; nothing
happened" without distinguishing the two cases. `1` is reserved for
"we tried; something broke" so CI can retry. Subcommands document
the matrix in their `--help` output.

For `djogi migrations apply`, drift refusals follow the same matrix:

- detected error-level drift before SQL runs: exit `2`
- missing snapshot for a previously-applied bucket: exit `2`
- drift pre-flight infrastructure failure: exit `1`

`db reset` hard-errors unless all three guards pass: `DATABASE_URL` resolves to localhost (per the byte-level libpq + URL parser shared with `attune --squash`), `Djogi.toml::profile != "production"`, and the operator supplies explicit confirmation (either `--yes` on the command line or types `yes` at the interactive prompt). After those guards, reset still runs a non-destructive checksum-parity preflight against the live ledger before `DROP DATABASE`: edited migration files, missing historical files, or baseline rows whose checksums cannot be compared to file bytes refuse with exit code `2` unless `--allow-checksum-drift-reset` is passed. Logging databases (`crud_log`, `event_log`) are NEVER touched by `db reset`.

`db seed` uses `--database <name>` to select BOTH the seed directory (`seeds/<name>/`) and the connection target. The CLI splices `<name>` into `database.url`'s path component (via `djogi::migrate::derive_per_database_url`) so seeds always run against the matching DB; a malformed application URL refuses with exit code 1 rather than falling back to the application database. Per-database routing is the linchpin of the three-database architecture (`url` / `crud_log_url` / `event_log_url`) — until config exposes per-DB URL fields directly, the splice gives operators a deterministic route to every cluster database from a single application URL.

`migrations attune` manages local migration-history Git state. It may fetch remote refs when needed to resolve a target, and `--apply` commits ledger/disk reconciliation changes, but it does not execute migration SQL or apply schema DDL. Parent-repo submodule-pointer changes are explicit via `--record` or options that clearly imply recording, such as `--squash`.

`migrations attune` target contract:

- target may be omitted, in which case Djogi attunes to the repo-default/expected migration-history state
- target may be a local or remote commit, tag, or branch
- if `migrations/` has no remote configured, attune is limited to locally available Git targets
- `--record` updates the parent repo's recorded submodule pointer after successful attunement
- `--squash` is hard-gated by the conjunction of FOUR conditions: localhost URL resolution, `Djogi.toml::profile != "production"`, `Djogi.toml::[database].dev_mode = true`, and `DJOGI_ENV` env var NOT case-insensitive `"production"`.
- `--squash` should refuse when the migration history is already treated as shared staging/production history
- `--squash --publish` requires a configured remote (the spec previously used `--push`; the CLI canonicalised on `--publish` per the OQ-04 ruling in `docs/spec/decisions.md` — `--publish` matches `cargo publish` vocabulary and avoids overloading git's `push` verb)
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
    State(pool): State<DjogiPool>,
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
- `HEER_NODE_ID` and `HEER_RANJ_NODE_ID` are selected via environment and must be provisioned in `heer_nodes` before service startup; wire them into the pool's `post_connect` hook
- Configures standard `tower` middleware (tracing, request ID, optional auth)
- Optionally runs pending migrations on boot (configurable; default on in dev, off in production)

Migration-on-boot behavior is framework-agnostic — they run whether or not a web-framework flag is enabled. Only the router-merging glue is Axum-specific here; equivalent glue ships under each future framework flag.
