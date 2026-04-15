> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

## 10. Migrations

### 10.1 Philosophy

- Migration files are plain SQL — readable, editable, reviewable
- Execution is sqlx's built-in runner — checksummed, tracked in `_sqlx_migrations`
- Generation happens automatically at build time via `build.rs`
- `schema_snapshot.json` is the source of truth for the differ — updated only on successful `cargo djogi migrate`
- Up and down files always generated as a pair
- The `migrations/` folder is a git submodule — managed by the pipeline, invisible to the developer day-to-day

### 10.2 Build-Time Drift Detection and Generation

`build.rs` runs on every `cargo build`:

1. Reads model descriptors from `target/djogi_models.json` (written as a side-effect of proc macro expansion via `inventory`)
2. Diffs against `migrations/schema_snapshot.json` (the current DB state)
3. If drift is detected:
   - Generates a migration pair (`NNNN_description_up.sql` / `NNNN_description_down.sql`) into `migrations/`
   - Emits a compiler warning (not an error) with the generated filenames
   - Build proceeds — the developer reviews and applies when ready
4. If no drift: build proceeds silently
Drift notification looks like a compiler diagnostic:
```
warning[D001]: schema drift detected — migration generated
  --> src/apps/vehicles/models.rs:8:9
   |
 8 |     pub horsepower: i32,
   |     ^^^^^^^^^^^^^^^^^^^ new field — no migration existed
   |
   = note: generated migrations/0005_add_vehicles_horsepower_up.sql
   = note: generated migrations/0005_add_vehicles_horsepower_down.sql
   = help: review the SQL, then run `cargo djogi migrate` when ready
```
Destructive operations (DROP COLUMN, DROP TABLE) emit a warning and require `--allow-destructive` to proceed:
```
warning[D002]: destructive migration requires confirmation
   = help: run `cargo djogi makemigrations --allow-destructive` to generate
```
### 10.3 Schema Snapshot

`migrations/schema_snapshot.json` represents what the database actually looks like — updated only when `cargo djogi migrate` succeeds. Never updated by the build step.
```json
{
  "version": "0005",
  "migrated_at": "2025-03-26T10:00:00Z",
  "models": {
    "vehicles": {
      "columns": [
        { "name": "id", "sql_type": "BIGINT", "nullable": false, "primary_key": true },
        { "name": "owner_id", "sql_type": "BIGINT", "nullable": false,
          "references": { "table": "owners", "column": "id", "on_delete": "restrict" } },
        { "name": "gas_fill", "sql_type": "INTEGER", "nullable": false }
      ],
      "indexes": [{ "name": "idx_vehicles_active", "columns": ["active"] }]
    }
  }
}
```
### 10.4 Generated Migration Pair
`migrations/0005_add_vehicles_horsepower_up.sql`:
```sql
-- Migration: 0005_add_vehicles_horsepower
-- Direction: UP
-- Generated: 2025-03-26T10:00:00Z

ALTER TABLE vehicles ADD COLUMN horsepower INTEGER NOT NULL DEFAULT 0;
CREATE INDEX idx_vehicles_horsepower ON vehicles (horsepower);
```
`migrations/0005_add_vehicles_horsepower_down.sql`:
```sql
-- Migration: 0005_add_vehicles_horsepower
-- Direction: DOWN
-- WARNING: dropping a column is irreversible — data is not recoverable on rollback

DROP INDEX idx_vehicles_horsepower;
ALTER TABLE vehicles DROP COLUMN horsepower;
```
Down migration generation rules:

| Up | Down | Notes |
|---|---|---|
| `CREATE TABLE` | `DROP TABLE` | |
| `DROP TABLE` | `CREATE TABLE` | Full definition re-emitted |
| `ADD COLUMN` | `DROP COLUMN` | |
| `DROP COLUMN` | `ADD COLUMN` | ⚠ Data unrecoverable |
| `CREATE INDEX` | `DROP INDEX` | |
| `DROP INDEX` | `CREATE INDEX` | |
| `ALTER COLUMN` | `ALTER COLUMN` reverse | ⚠ Warn if lossy |
| `ADD FOREIGN KEY` | `DROP FOREIGN KEY` | |

### 10.5 Migrations Git Submodule

The `migrations/` directory is a git submodule. The pipeline owns it:
```yaml
ci:
  - cargo build              # generates any missing migration files
  - cargo djogi migrate      # applies SQL, updates schema_snapshot.json on success
  - cd migrations && git add . && git commit -m "migrate: $(date)" && git push
```
Locally the developer never touches the submodule directly. Migration files appear in their editor after `cargo build`, they review the SQL, then run `cargo djogi migrate` when ready.
### 10.6 Differ
```rust
enum SchemaDelta {
    CreateTable { table: TableDef },
    DropTable { name: String },
    AddColumn { table: String, column: ColumnDef },
    DropColumn { table: String, name: String },
    AlterColumn { table: String, name: String, change: ColumnChange },
    AddIndex { table: String, index: IndexDef },
    DropIndex { name: String },
    AddForeignKey { table: String, fk: ForeignKeyDef },
    DropForeignKey { table: String, name: String },
}
```
Field renames treated as drop+add unless annotated with `#[field(renamed_from = "old_name")]`.
---
