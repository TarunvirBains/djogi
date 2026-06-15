> [Back to roadmap index](./index.md) | [Shipped guides](../guide/index.md)

# CLI Roadmap — current binary is `djogi`

> **Status: MOSTLY SHIPPED.** The shipped CLI provides
> `djogi migrations apply` (with `--fake` / `--reason`), `compose`, `status`,
> `attune`, `verify`, `repair`, `baseline`, and `rollback`, plus
> `djogi db reset / seed` and `djogi docs`. The
> authoritative current CLI surface lives in
> [`docs/guide/migrations.md`](../guide/migrations.md). This roadmap
> document is preserved as design history.

`djogi` is the shipped Djogi command-line interface. This roadmap also preserves historical and planned command sketches; use the shipped guides for authoritative current syntax. Install it once:

```bash
cargo install djogi-cli
```

All subcommands run from the project root (the directory containing `Djogi.toml`). The CLI reads `Djogi.toml` and the `DATABASE_URL`, `DJOGI_ENV` environment variables.

**Migration-runner identity.** Identity-bearing commands (`migrations apply`, `migrations rollback`, `migrations baseline`, `db reset`, `repair resume-partial`) support `--node-id <id>` and `--single-node-dev` flags. The CLI resolver selects explicit `--node-id` over `HEER_NODE_ID`; values outside `0..=511` refuse with exit code 2. `--single-node-dev` is refused under production profile (`DJOGI_ENV=production`). Runtime application pools remain caller-owned via `post_connect` and do NOT read `HEER_NODE_ID`.

---

## Migration Commands

### `djogi migrations apply`

Applies all pending migration files to the database in ledger order, then updates `migrations/schema_snapshot.json` to reflect the new DB state. Also available as `djogi migrate apply`.

```bash
djogi migrations apply
djogi migrations apply --fake --reason "schema pre-exists from prior tooling"
```

**Flags:**

| Flag | Description |
|---|---|
| `--fake` | Mark pending migrations as applied without executing their SQL. For existing-database adoption only. Requires `--reason`. Respects out-of-order policy gates. |
| `--reason TEXT` | Required when `--fake` is set. Persisted to the ledger audit trail. |

> **Warning:** `--fake` bypasses the migration runner entirely. Use it only when you have applied the SQL by other means (direct `psql`, cloud console, etc.) and need to bring the tracking table in sync. Verify the live schema matches the target state with `djogi migrations verify` or manual inspection before faking.

**Exit codes:** `0` on success, `1` on runtime error (config / network / SQL), `2` on refusal (policy gate failure or argument validation).

---

### `djogi migrations rollback`

Rolls back the newest applied migration in the selected bucket, or every
applied migration newer than `--to <version>`, in reverse ledger insertion
order. The command executes the committed `<version>.down.sdjql` file, flips
the matching ledger row to `rolled_back`, then re-projects
`schema_snapshot.json` from the live database whenever at least one rollback
committed.

```bash
djogi migrations rollback --single-node-dev
djogi migrations rollback --to V20260101000000__init --node-id 7
djogi migrations rollback --dry-run
```

**Flags:**

| Flag | Description |
|---|---|
| `--to <version>` | Keep `<version>` applied and roll back every newer applied row in reverse ledger insertion order |
| `--dry-run` | Print the committed down SQL that would run without executing it |
| `--allow-data-loss` | Opt in to lossy down SQL flagged by committed `-- LOSSY` markers; requires `--reason` |
| `--reason TEXT` | Required with `--allow-data-loss`; persisted into the ledger note for audit |
| `--app <label>` | Select a non-global app bucket |
| `--database <name>` | Select the bucket database (defaults to `main`) |
| `--workspace <path>` | Override the workspace root |
| `--node-id <id>` / `--single-node-dev` | Bind runner identity for non-dry-run rollback execution |

The `--dry-run` preview reflects the current ledger state; the real command
re-reads the ledger after acquiring the workspace lock and refuses with exit
code `2` if the target set changed while it was waiting for the lock.

Exit codes: `0` success, `1` runtime error (config / network / SQL /
down-statement failure), `2` refusal (lossy without opt-in, missing or
non-rollbackable version, checksum drift, non-transactional down SQL, below
Postgres 18, or ledger drift while waiting for the lock).

> **Warning:** Down migrations for column drops and table drops are destructive — data is not recoverable after rollback. Each down migration file includes a comment warning about data loss. Review the down file before executing `rollback`.

---

### `djogi plan`

Shows a human-readable summary of pending migrations without applying them.

```bash
djogi plan
```

**Example output:**

```
Pending migrations (2):

 0004_add_articles_rating
 + ADD COLUMN rating DOUBLE PRECISION
 table: articles

 0005_create_tags
 + CREATE TABLE tags (3 columns)
 id BIGINT PK, name VARCHAR(100), slug VARCHAR(100)

Use `djogi migrations apply` (or the library entry point `djogi::migrate::apply_plan`) to apply migration
plans.
```

If there are no pending migrations:

```
No pending migrations. Schema is at version 0003.
```

---

### `djogi migrations compose`

Manually triggers migration generation from current schema drift. Under normal development, `build.rs` generates migration files automatically on every `cargo build`. Use `makemigrations` when you need dry-run output, a custom name, or to explicitly permit destructive operations.

```bash
djogi migrations compose
djogi migrations compose --allow-destructive # permit DROP COLUMN / DROP TABLE
djogi migrations compose --name "backfill_nulls" # custom migration name
```

| Flag | Description |
|---|---|
| `--allow-destructive` | Permit destructive operations (DROP COLUMN, DROP TABLE) in the generated SQL |
| `--name TEXT` | Use a custom description in the migration filename |

---

### `djogi verify`

Verifies repository/package state through the shipped top-level `djogi verify` command. For migration-specific live-DB verification, use `djogi migrations verify` (shipped) which compares `schema_snapshot.json` against the live DB and reports diagnostics. The `--strict` flag promotes out-of-order diagnostics from Warning to Error.

```bash
djogi verify
djogi migrations verify
djogi migrations verify --strict
```

**Example output (valid):**

```
Verifying schema snapshot signature...
OK: schema_snapshot.json is authentic (signed at 2026-04-15T10:00:00Z)
```

**Example output (tampered):**

```
ERROR: schema snapshot signature mismatch
 Expected: 8f3c2a1b4e7d9f2c...
 Found: 7e1d4f9c3a8b1e5d...

The schema snapshot may have been modified outside of Djogi's migration library.
Do not run migrations until this is resolved.
```

---

## Shell

### `djogi shell`

> **Current status:** planned (a future release), deferred in v0.1.0 shipped CLI. This section documents the target behavior.

Starts an interactive Rhai REPL with all registered models pre-loaded, a live database connection, and persistent command history.

```bash
djogi shell
```

The shell holds a dedicated single-threaded Tokio runtime. All terminal methods (`fetch_all`, `fetch_one`, `save`, `delete`, `create`, etc.) execute synchronously — no `.await`, no async ceremony. Blocking is intentional — the shell is for interactive exploration.

**Example session:**

```
djogi shell v0.1.0
Database: postgres://localhost/myapp (version 0003)
Models: Post, Comment, User, Tag
Type.help for available commands.

djogi> let posts = Post::objects()
 .filter_struct(PostFilter::new().published(Eq(true)))
 .order_by_desc("created_at")
 .limit(5)
 .fetch_all();
djogi> pp(posts)

┌────────────────┬──────────────────────┬───────────┐
│ id  │ title  │ published │
├────────────────┼──────────────────────┼───────────┤
│ 7493920192847 │ Getting Started │ true │
│ 7493920192001 │ Model-first Rust │ true │
└────────────────┴──────────────────────┴───────────┘

djogi> let post = Post::get(7493920192847)
djogi> post.view_count = post.view_count + 1
djogi> post.save()
djogi>
```

**Error handling:**

Errors print a one-liner and save a full traceback to `.djogi_shell_errors/`. The session is never unwound — local variables and open transactions survive errors.

```
djogi> let x = Post::get(99999)
Error: record not found (posts where id = 99999)
 → traceback saved to.djogi_shell_errors/2026-04-15T10-42-11_001.log

djogi> ← session continues
```

**Transaction control:**

```
djogi> begin()
Transaction timeout [default: 30m, or enter duration e.g. 1h, or leave blank for none]: _
Note: uncommitted work will be lost if the shell exits or loses connection.
djogi (txn)> let post = Post::get(42)
djogi (txn)> post.title = "Updated"
djogi (txn)> post.save()
djogi (txn)> commit()
djogi>
```

**Savepoints:**

```
djogi (txn)> savepoint("checkpoint")
djogi (txn)> // risky work here
djogi (txn)> rollback_to("checkpoint")
djogi (txn)> commit()
djogi>
```

**Shell built-in commands:**

| Command | Description |
|---|---|
| `pp(value)` | Print an ASCII table (for collections) or key-value list (for single models) |
| `sql("...")` | Execute raw SQL — returns an array of dynamic maps |
| `begin()` | Open a transaction, prompts for optional timeout |
| `commit()` | Commit the open transaction |
| `rollback()` | Roll back the open transaction |
| `savepoint("name")` | Create a named savepoint |
| `rollback_to("name")` | Roll back to a named savepoint |
| `reload()` | Re-initialize model bindings (after code changes) |
| `.help` | Print available commands and models |
| `.export name` | Save current session history to `scripts/name.rhai` |
| `.export name --from bookmark` | Save history from a named bookmark position |
| `.bookmark name` | Bookmark the current history position |
| `.import name` | Run `scripts/name.rhai` inside the current session |
| `.clear_errors` | Delete all logs in `.djogi_shell_errors/` |

**Flags:**

| Flag | Description |
|---|---|
| `--run scripts/name.rhai` | Run a script headlessly without entering the REPL |
| `--verbose` | Print full tracebacks inline in addition to saving to disk |
| `--database-url URL` | Override `DATABASE_URL` for this shell session |

**Headless execution:**

```bash
djogi shell --run scripts/backfill_slugs.rhai
```

This runs the script in the full shell environment and exits. Useful for CI pipelines, scheduled jobs, and one-off data transformations.

**Session import/export:**

Export the current session's meaningful history as a reusable script:

```
djogi>.bookmark before_backfill
djogi> //... do some work...
djogi>.export backfill_slugs --from before_backfill
Saved to scripts/backfill_slugs.rhai
```

Raw navigation (up-arrow corrections, typos) is filtered out automatically.

---

## Database Commands

All `db` subcommands are gated on three guards: `dev_mode = true` in `Djogi.toml`, `DATABASE_URL` resolving to localhost, and `DJOGI_ENV != production`. Any guard failing causes a hard error.

### `djogi db reset`

Drops and recreates the application database, then applies all migrations from scratch. The CRUD log and event log databases are not touched.

```bash
djogi db reset
```

**Example output:**

```
Checking safety guards...
 dev_mode = true ✓
 localhost URL  ✓
 DJOGI_ENV = development ✓

Dropping myapp...
Creating myapp...
Installing HeeRanjId schema...
Applying 0001_create_posts... done
Applying 0002_add_posts_view_count... done
Applying 0003_create_comments... done

Database reset complete. Schema at version 0003.
```

**Flags:**

| Flag | Description |
|---|---|
| `--wipe-crud-logs` | Also drop and recreate the CRUD log database |
| `--wipe-all-logs` | Also drop and recreate both log databases |

```bash
djogi db reset --single-node-dev --yes
djogi db seed
# planned: djogi db reset --wipe-crud-logs
```

### `djogi db seed`

Runs operator-authored SQL seed files in `seeds/<database>/*.sql` alphabetically against the existing database without dropping or migrating. The runner takes a per-database advisory lock, records each seed run in the `djogi_seed_runs` ledger, and skips files whose `V1:<sha256>` checksum already matches an `applied` row.

```bash
djogi db seed
```

---

## Analysis and Maintenance

### `djogi analyze`

Generates a table health report for all models registered in the application. Reports on VACUUM needs, bloat estimates, missing indexes, and partition recommendations.

```bash
djogi analyze
```

**Example output:**

```
Table Health Report — 2026-04-15

posts (1,240,482 rows)
 Bloat estimate: 12% (acceptable)
 Last autovacuum: 2026-04-15T08:12:00Z
 Recommendation: none

comments (8,903,211 rows)
 Bloat estimate: 31% (elevated)
 Last autovacuum: 2026-04-14T23:00:00Z
 Recommendation: VACUUM ANALYZE comments
 Missing index: filter on (post_id, created_at) detected in slow query log

events (24,000,000 rows, partitioned by range:occurred_at)
 Partitions: 12 active, 2 detachable
 Recommendation: Consider detaching partitions older than 2025-10-01
```

**Flags:**

| Flag | Description |
|---|---|
| `--table TABLE` | Analyze a specific table only |
| `--json` | Output raw JSON for CI integration |
| `--threshold-bloat N` | Set the bloat percentage threshold for recommendations (default: 20) |

---

### `djogi repartition`

Generates zero-downtime repartition SQL for a partitioned table. Djogi produces a script using the `CREATE TABLE... PARTITION OF` pattern with concurrent index builds — no table lock for reads.

```bash
djogi repartition events --from "range:occurred_at" --to "hash:user_id:16"
```

**Example output:**

```
Generating repartition SQL for events...

-- Step 1: Create new partitioned table alongside the existing one
-- Step 2: Backfill data in batches (no lock on reads)
-- Step 3: Swap tables atomically
-- Step 4: Drop old table

Written to scripts/repartition_events_2026-04-15.sql

Review carefully before running. Run with:
 djogi shell --run scripts/repartition_events_2026-04-15.sql
```

> **Warning:** Repartition scripts are generated for review, not automatic execution. Always inspect the SQL and test on a staging environment before running on production data.

---

### `djogi rls`

Generates Row Level Security policy SQL for all models annotated with `#[model(tenant_key = "...")]`. Useful for reviewing what policies will be applied before running `djogi migrations apply`.

```bash
djogi rls
```

**Example output:**

```sql
-- RLS Policies for tenant-keyed models
-- Generated: 2026-04-15T10:00:00Z

-- Model: Invoice (table: invoices, tenant_key: org_id)

ALTER TABLE invoices ENABLE ROW LEVEL SECURITY;
ALTER TABLE invoices FORCE ROW LEVEL SECURITY;

CREATE POLICY invoices_tenant_isolation ON invoices
 USING (org_id = current_setting('djogi.tenant_id')::bigint)
 WITH CHECK (org_id = current_setting('djogi.tenant_id')::bigint);
```

**Flags:**

| Flag | Description |
|---|---|
| `--model MODEL` | Generate policy for a specific model only |
| `--apply` | Apply the generated policies immediately (runs as part of `migrate` normally) |

---

## Documentation Commands

### `djogi docs`

Generates Markdown documentation from the `ModelDescriptor` inventory for all registered models. Output goes to `docs/models/`.

```bash
djogi docs
```

**Example output:**

```
Generating model documentation...

 docs/models/Post.md
 docs/models/Comment.md
 docs/models/User.md
 docs/models/Invoice.md

4 files written to docs/models/.
```

Each generated file includes: table name, PK type, all fields with their types and annotations, `#[model(...)]` attributes, `#[field(rationale)]` strings, and the full M2M relationship graph.

**Flags:**

| Flag | Description |
|---|---|
| `--output-dir DIR` | Write output to a different directory (default: `docs/models/`) |
| `--format md\|json` | Output format — Markdown (default) or JSON |

---

### `djogi check-docs`

Validates that `docs/models/*.md` files are consistent with the live `ModelDescriptor` inventory. Detects documentation that has drifted from the actual model definition — fields added or removed without updating the doc.

```bash
djogi check-docs
```

**Example output (all valid):**

```
Checking docs/models/ against live ModelDescriptor inventory...

 Post.md ✓ up to date
 Comment.md ✓ up to date
 User.md ✓ up to date

All docs are consistent.
```

**Example output (drift detected):**

```
Checking docs/models/ against live ModelDescriptor inventory...

 Invoice.md ✗ drift detected

 Invoice.md documents: total_cents, status, due_date
 ModelDescriptor has: total_cents, status, due_date, paid_at (MISSING FROM DOCS)

Run `djogi docs` to regenerate.
```

Use `check-docs` in CI to ensure documentation stays current with the model definitions.

---

## Project Scaffolding

### `djogi new my-project`

Scaffolds a new Djogi project with the standard layout, initializes the `migrations/` git submodule, and creates starter files.

```bash
djogi new my-project
```

**Example output:**

```
Creating my-project/
 my-project/Cargo.toml
 my-project/build.rs
 my-project/Djogi.toml
 my-project/docker-compose.yml
 my-project/src/main.rs
 my-project/src/apps/mod.rs
 my-project/seeds/my_project/
 my-project/.gitignore
 my-project/migrations/ (git submodule initialized)

Project created. Next steps:
 cd my-project
 docker compose up -d
 export DATABASE_URL="postgres://djogi:djogi@localhost/my_project"
 djogi db reset --single-node-dev --yes
 djogi db seed --database my_project
```

### `djogi init`

Adds Djogi to an existing Rust project. Updates `Cargo.toml`, creates `Djogi.toml`, `build.rs`, and the `migrations/` submodule without touching `src/`.

```bash
cd existing-project
djogi init
```
