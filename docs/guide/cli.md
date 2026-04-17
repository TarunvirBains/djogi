> [Back to README](../../ReadMe.MD) | [All Guides](./index.md)

# CLI Reference — `cargo djogi`

`cargo djogi` is the Djogi command-line interface. Install it once:

```bash
cargo install djogi-cli
```

All subcommands run from the project root (the directory containing `Djogi.toml`). The CLI reads `Djogi.toml` and the `DATABASE_URL`, `NODE_ID`, and `DJOGI_ENV` environment variables.

---

## Migration Commands

### `cargo djogi migrate`

Applies all pending migration files to the database, then updates `migrations/schema_snapshot.json` to reflect the new DB state.

```bash
cargo djogi migrate
```

**What it does:**

1. Reads all `.sql` files in the `migrations/` directory
2. Checks which migrations have already been applied (tracked in `_sqlx_migrations`)
3. Applies pending migrations in order, inside individual transactions
4. On success, updates `schema_snapshot.json` to the new version
5. Writes a DDL audit entry to the event log database

**Example output:**

```
Applying 0001_create_posts... done
Applying 0002_add_posts_view_count... done
Applying 0003_create_comments... done

3 migrations applied. Schema at version 0003.
Schema snapshot updated.
```

If a migration fails, the transaction is rolled back, the snapshot is not updated, and the error is printed with the failing SQL highlighted.

**Flags:**

| Flag | Description |
|---|---|
| `--fake N` | Mark migration N as applied without running its SQL (use when manually applying migrations outside Djogi) |
| `--dry-run` | Show what would be applied without executing |
| `--database-url URL` | Override `DATABASE_URL` for this invocation |

```bash
cargo djogi migrate --dry-run
cargo djogi migrate --fake 0003
```

> **Warning:** `--fake` bypasses the migration runner entirely. Use it only when you have applied the SQL by other means (direct `psql`, cloud console, etc.) and need to bring the tracking table in sync. Misuse can leave the snapshot and actual DB state out of sync.

---

### `cargo djogi rollback`

Rolls back the last applied migration by running its `_down.sql` pair, then rewinds `schema_snapshot.json` to the previous version.

```bash
cargo djogi rollback
```

**Example output:**

```
Rolling back 0003_create_comments... done
Schema rewound to version 0002.
```

**Flags:**

| Flag | Description |
|---|---|
| `--to N` | Roll back to a specific version (applies multiple down migrations in reverse order) |
| `--dry-run` | Show the SQL that would run without executing |

```bash
cargo djogi rollback --to 0001
cargo djogi rollback --dry-run
```

> **Warning:** Down migrations for column drops and table drops are destructive — data is not recoverable after rollback. Each down migration file includes a comment warning about data loss. Review the down file before executing `rollback`.

---

### `cargo djogi plan`

Shows a human-readable summary of pending migrations without applying them.

```bash
cargo djogi plan
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

Run `cargo djogi migrate` to apply.
```

If there are no pending migrations:

```
No pending migrations. Schema is at version 0003.
```

---

### `cargo djogi makemigrations`

Manually triggers migration generation from current schema drift. Under normal development, `build.rs` generates migration files automatically on every `cargo build`. Use `makemigrations` when you need dry-run output, a custom name, or to explicitly permit destructive operations.

```bash
cargo djogi makemigrations
cargo djogi makemigrations --dry-run          # preview SQL without writing files
cargo djogi makemigrations --allow-destructive  # permit DROP COLUMN / DROP TABLE
cargo djogi makemigrations --name "backfill_nulls"  # custom migration name
```

| Flag | Description |
|---|---|
| `--dry-run` | Print the SQL that would be generated without writing migration files |
| `--allow-destructive` | Permit destructive operations (DROP COLUMN, DROP TABLE) in the generated SQL |
| `--name TEXT` | Use a custom description in the migration filename |

---

### `cargo djogi verify`

Verifies the HMAC-SHA256 signature of `migrations/schema_snapshot.json`. Requires `DJOGI_SIGNING_KEY` to be set.

```bash
cargo djogi verify
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
  Found:    7e1d4f9c3a8b1e5d...

The schema snapshot may have been modified outside of `cargo djogi migrate`.
Do not run migrations until this is resolved.
```

**Flags:**

| Flag | Description |
|---|---|
| `--explain` | Print the full signature details and the content hash for debugging |

---

## Shell

### `cargo djogi shell`

Starts an interactive Rhai REPL with all registered models pre-loaded, a live database connection, and persistent command history.

```bash
cargo djogi shell
```

The shell holds a dedicated single-threaded Tokio runtime. All terminal methods (`fetch_all`, `fetch_one`, `save`, `delete`, `create`, etc.) execute synchronously — no `.await`, no async ceremony. Blocking is intentional — the shell is for interactive exploration.

**Example session:**

```
djogi shell v0.1.0
Database: postgres://localhost/myapp (version 0003)
Models: Post, Comment, User, Tag
Type .help for available commands.

djogi> let posts = Post::objects()
           .filter_struct(PostFilter::new().published(Eq(true)))
           .order_by_desc("created_at")
           .limit(5)
           .fetch_all();
djogi> pp(posts)

┌────────────────┬──────────────────────┬───────────┐
│ id             │ title                │ published │
├────────────────┼──────────────────────┼───────────┤
│ 7493920192847  │ Getting Started      │ true      │
│ 7493920192001  │ Model-first Rust     │ true      │
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
  → traceback saved to .djogi_shell_errors/2026-04-15T10-42-11_001.log

djogi>   ← session continues
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
cargo djogi shell --run scripts/backfill_slugs.rhai
```

This runs the script in the full shell environment and exits. Useful for CI pipelines, scheduled jobs, and one-off data transformations.

**Session import/export:**

Export the current session's meaningful history as a reusable script:

```
djogi> .bookmark before_backfill
djogi> // ... do some work ...
djogi> .export backfill_slugs --from before_backfill
Saved to scripts/backfill_slugs.rhai
```

Raw navigation (up-arrow corrections, typos) is filtered out automatically.

---

## Database Commands

All `db` subcommands are gated on three guards: `dev_mode = true` in `Djogi.toml`, `DATABASE_URL` resolving to localhost, and `DJOGI_ENV != production`. Any guard failing causes a hard error.

### `cargo djogi db reset`

Drops and recreates the application database, then applies all migrations from scratch. The CRUD log and event log databases are not touched.

```bash
cargo djogi db reset
```

**Example output:**

```
Checking safety guards...
  dev_mode = true      ✓
  localhost URL         ✓
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
| `--seed` | Run `seeds.rhai` after migrations complete |
| `--wipe-crud-logs` | Also drop and recreate the CRUD log database |
| `--wipe-all-logs` | Also drop and recreate both log databases |

```bash
cargo djogi db reset --seed
cargo djogi db reset --wipe-crud-logs
```

### `cargo djogi db seed`

Runs `seeds.rhai` against the existing database without dropping or migrating. The seed script runs inside a transaction — if any step fails, the entire seed is rolled back.

```bash
cargo djogi db seed
```

---

## Analysis and Maintenance

### `cargo djogi analyze`

Generates a table health report for all models registered in the application. Reports on VACUUM needs, bloat estimates, missing indexes, and partition recommendations.

```bash
cargo djogi analyze
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

### `cargo djogi repartition`

Generates zero-downtime repartition SQL for a partitioned table. Djogi produces a script using the `CREATE TABLE ... PARTITION OF` pattern with concurrent index builds — no table lock for reads.

```bash
cargo djogi repartition events --from "range:occurred_at" --to "hash:user_id:16"
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
  cargo djogi shell --run scripts/repartition_events_2026-04-15.sql
```

> **Warning:** Repartition scripts are generated for review, not automatic execution. Always inspect the SQL and test on a staging environment before running on production data.

---

### `cargo djogi rls`

Generates Row Level Security policy SQL for all models annotated with `#[model(tenant_key = "...")]`. Useful for reviewing what policies will be applied before running `cargo djogi migrate`.

```bash
cargo djogi rls
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

### `cargo djogi docs`

Generates Markdown documentation from the `ModelDescriptor` inventory for all registered models. Output goes to `docs/models/`.

```bash
cargo djogi docs
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

### `cargo djogi check-docs`

Validates that `docs/models/*.md` files are consistent with the live `ModelDescriptor` inventory. Detects documentation that has drifted from the actual model definition — fields added or removed without updating the doc.

```bash
cargo djogi check-docs
```

**Example output (all valid):**

```
Checking docs/models/ against live ModelDescriptor inventory...

  Post.md       ✓  up to date
  Comment.md    ✓  up to date
  User.md       ✓  up to date

All docs are consistent.
```

**Example output (drift detected):**

```
Checking docs/models/ against live ModelDescriptor inventory...

  Invoice.md    ✗  drift detected

    Invoice.md documents:  total_cents, status, due_date
    ModelDescriptor has:   total_cents, status, due_date, paid_at (MISSING FROM DOCS)

Run `cargo djogi docs` to regenerate.
```

Use `check-docs` in CI to ensure documentation stays current with the model definitions.

---

## Project Scaffolding

### `cargo djogi new my-project`

Scaffolds a new Djogi project with the standard layout, initializes the `migrations/` git submodule, and creates starter files.

```bash
cargo djogi new my-project
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
  my-project/seeds.rhai
  my-project/.gitignore
  my-project/migrations/  (git submodule initialized)

Project created. Next steps:
  cd my-project
  docker compose up -d
  export DATABASE_URL="postgres://djogi:djogi@localhost/my_project"
  export NODE_ID=1
  cargo djogi db reset --seed
```

### `cargo djogi init`

Adds Djogi to an existing Rust project. Updates `Cargo.toml`, creates `Djogi.toml`, `build.rs`, and the `migrations/` submodule without touching `src/`.

```bash
cd existing-project
cargo djogi init
```
