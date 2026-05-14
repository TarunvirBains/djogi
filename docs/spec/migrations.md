> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

## 10. Migrations

### 10.1 Philosophy

Djogi's migration system is built around four priorities, in order:

1. scalability for long-lived applications
2. production stability under failure conditions
3. ease of use for developers and operators
4. idiomatic Rust without hiding schema changes inside opaque Rust code

The core contract is:

- model descriptors are the desired-state source of truth
- SQL files are the primary human review artifact
- `schema_snapshot.json` is the applied-schema side-car, not a live-catalog dump
- the migration runner is Djogi-owned, not `sqlx::migrate`
- `build.rs` detects drift but does not write migration files
- up and down files are always generated as a pair
- the `migrations/` folder is a git submodule managed as migration history
- composite unique constraints and composite indexes are part of the migration surface
- composite primary keys are not part of the `0.1.0` contract

Djogi treats migration state as three distinct truths:

1. desired schema — derived from model descriptors
2. applied schema model — stored in `schema_snapshot.json`
3. operational history — stored in migration files plus the database ledger

These truths must remain separate.

### 10.2 Drift Detection and Generation

`build.rs` runs on every `cargo build` and is **diagnostic-only**.

It may:

1. read model descriptors from `target/djogi_models.json`
2. read committed snapshots from `migrations/<target>/<app>/schema_snapshot.json`
3. read pending target-state files from `target/djogi_pending/<target>/<app>.json`
4. emit a plain cargo warning when drift is detected

It must **not**:

- write SQL migration files
- mutate snapshots
- mutate the migrations submodule

Generation is explicit via CLI:

```bash
djogi migrations compose
djogi migrations compose --dry-run
djogi migrations compose --allow-destructive
djogi migrations compose --name add_vehicle_horsepower
```

Three-way match logic, run per `(target, app)` pair:

1. `djogi_models.json == schema_snapshot.json` for every `(target, app)` pair → silent
2. descriptor/snapshot mismatch, but matching `target/djogi_pending/<target>/<app>.json` exists → warn that a migration is pending apply
3. descriptor/snapshot mismatch and no matching pending file exists → warn that schema drift exists and `migrations compose` should be run

Example build warning:

```text
warning: djogi: schema drift detected — run `djogi migrations compose`
```

### 10.3 Snapshot Model

Djogi uses three file roles:

| File | Location | Role | Committed? |
|---|---|---|---|
| `target/djogi_models.json` | build artifact | what Rust source currently declares | no |
| `target/djogi_pending/<target>/<app>.json` | build artifact | generated target state waiting to be applied | no |
| `migrations/<target>/<app>/schema_snapshot.json` | migrations submodule | last known applied schema shape for that `(target, app)` pair | yes |

`schema_snapshot.json` is updated exactly once per successful apply flow, via atomic file replacement (`tmp -> fsync -> rename`). It is never updated by `build.rs`.

Snapshot format includes at minimum:

```json
{
  "format_version": 1,
  "version": "0005_add_vehicle_horsepower",
  "migrated_at": "2026-04-22T10:00:00Z",
  "models": {}
}
```

If a non-transactional migration fails partway through, the snapshot remains unchanged. Partial state is represented by the ledger and the local failure marker file, not by writing an intermediate snapshot.

Failure marker protocol:

- file path: `migrations/.migration_failure.json`
- written only after partial non-transactional failure
- blocks further planning/apply until resolved by `migrations repair`

### 10.4 Generated SQL Contract

Migration files are plain SQL and always generated as a pair:

- up file: `V<YYYYMMDDHHMMSS>__<slug>.sql`
- down file: `V<YYYYMMDDHHMMSS>__<slug>.down.sql`

The leading `V` plus 14-digit UTC timestamp gives lexical = chronological ordering across versions; the slug is operator-supplied (sanitised through the byte-level rules documented in `djogi::migrate::naming::sanitize_slug`). Example pair:

- `V20260425010203__add_vehicle_horsepower.sql`
- `V20260425010203__add_vehicle_horsepower.down.sql`

Example UP file:

```sql
-- Migration: V20260425010203__add_vehicle_horsepower
-- Direction: UP
-- Execution-Mode: transactional
-- Generated: 2026-04-25T01:02:03Z

ALTER TABLE vehicles ADD COLUMN horsepower INTEGER NOT NULL DEFAULT 0;
CREATE INDEX vehicles_horsepower_idx ON vehicles (horsepower);
```

Example DOWN file:

```sql
-- Migration: V20260425010203__add_vehicle_horsepower
-- Direction: DOWN
-- Execution-Mode: transactional
-- WARNING: dropping a column is irreversible — schema shape can be restored, data cannot

DROP INDEX vehicles_horsepower_idx;
ALTER TABLE vehicles DROP COLUMN horsepower;
```

If a file must run outside a transaction, it carries the directive:

```sql
-- djogi:no-transaction
```

Directive rules:

- must appear on the first non-blank, non-comment line of the file
- applies to the entire file
- `_up.sql` and `_down.sql` are evaluated independently
- generated automatically when Djogi knows the file must be non-transactional

If the runner detects a statement Postgres forbids inside a transaction and the directive is missing, execution fails before any SQL runs.

Destructive generated SQL uses `-- DJOGI WARNING:` comments in the UP file so code review sees the risk in the forward path, not only in rollback text.

### 10.5 Multi-Database Scope

Djogi's migration architecture explicitly accounts for multiple database targets over time.

Examples include:

- the primary application database
- the CRUD log database
- the event log database
- future service-owned databases

The execution contract remains strict:

- one migration plan applies to one database target at a time
- each database target has its own ledger
- each database target has its own snapshot set
- advisory locking is per target
- repair, baseline, verify, and apply are per target
- cross-database foreign keys are explicitly rejected

Djogi does **not** promise distributed atomic migration across multiple databases. Cross-target coordination is an orchestration concern, not a single transactional migration guarantee.

If the apps/database-domains subsystem is enabled, migrations may be grouped by `(database_target, app_label)` rather than only by a global flat scope. See [Apps & Database Domains](./apps-and-database-domains.md).

### 10.6 Differ Surface

The migration differ works from descriptors and snapshots, not by replaying the live database catalog on every build.

The public differ surface is:

```rust
enum SchemaDelta {
    CreateTable { table: TableDef },
    DropTable { name: String },
    RenameTable { old_name: String, new_name: String },

    AddColumn { table: String, column: ColumnDef },
    DropColumn { table: String, name: String },
    AlterColumn { table: String, name: String, change: ColumnChange },
    RenameColumn { table: String, old_name: String, new_name: String },

    AddUniqueConstraint { table: String, constraint: UniqueConstraintDef },
    DropUniqueConstraint { table: String, name: String },

    AddIndex { table: String, index: IndexDef },
    DropIndex { name: String },

    AddForeignKey { table: String, fk: ForeignKeyDef },
    DropForeignKey { table: String, name: String },

    CreateEnum { name: String, variants: Vec<String> },
    AlterEnum { name: String, change: EnumChange },
    DropEnum { name: String },

    CreateExtension { name: String, version: Option<String> },
    DropExtension { name: String },
}
```

Rename behavior is explicit only:

- `#[field(renamed_from = "old_name")]`
- `#[model(renamed_from = "old_table")]`

No heuristic rename guessing is part of the core differ.

### 10.6.1 Type-Derived CHECK Projection (djogi#186)

The differ projects a table-level `CHECK` constraint for every column whose
Rust source type widens to a Postgres column type. The projection is
type-driven and runs at descriptor → snapshot lowering time, so the resulting
CHECK serializes into `schema_snapshot.json` and survives every round-trip.

**Mapping table.** Each Rust source type widens to the smallest signed Postgres
integer that fits its full value range; `u64` widens to `NUMERIC(20, 0)`
because `u64::MAX > i64::MAX`. The CHECK expression bounds the widened column
to the Rust source type's natural range.

| Rust source | Postgres column | Type-derived CHECK expression          |
|-------------|-----------------|----------------------------------------|
| `i8`        | `SMALLINT`      | `<col> >= -128 AND <col> <= 127`       |
| `u8`        | `SMALLINT`      | `<col> >= 0 AND <col> <= 255`          |
| `u16`       | `INTEGER`       | `<col> >= 0 AND <col> <= 65535`        |
| `u32`       | `BIGINT`        | `<col> >= 0 AND <col> <= 4294967295`   |
| `u64`       | `NUMERIC(20, 0)`| `<col> >= 0 AND <col> <= 18446744073709551615` |

Identity-mapped widths (`i16`, `i32`, `i64`, `bool`, `String`, `f32`, `f64`,
...) project no CHECK because the column type already covers their full range.
FK columns inherit the parent PK's identity-width type, so they project no
CHECK either.

**Constraint naming.** Each projected CHECK becomes a table-level constraint
named `<table>_<column>_check`, deterministic from `(table, column)`. The
naming function lives at `djogi/src/migrate/sql.rs::check_constraint_name` and
truncates to Postgres' 63-byte identifier limit by appending an 8-char hex
digest to a 54-byte stem.

**ADD lifecycle.** Descriptor evolves from a column with no CHECK (e.g. `i64`)
to a column whose Rust source projects a CHECK (e.g. `u32`). The differ at
`migrate/diff.rs::emit_alter_column` emits `ColumnChange::SetCheck(Some(expr))`
which the SQL emitter renders as:

```sql
ALTER TABLE <table> ADD CONSTRAINT <table>_<column>_check CHECK (<expr>);
```

**DROP lifecycle.** Descriptor evolves from a column whose Rust source projects
a CHECK (e.g. `u32`) to a column with no CHECK (e.g. `i64`). The differ emits
`ColumnChange::SetCheck(None)` which renders as:

```sql
ALTER TABLE <table> DROP CONSTRAINT <table>_<column>_check;
```

The down-migration carries a documented-not-recoverable comment because the
prior CHECK expression is not preserved in the diff.

**AMEND lifecycle.** Descriptor evolves from a column with one CHECK to a
column with a different CHECK (e.g. `u16` → `u32`, or any `#[field(check)]`
expression edit). The differ detects the AMEND case explicitly and emits two
`ColumnChange` entries in order — `SetCheck(None)` followed by
`SetCheck(Some(new))` — so the SQL pair is:

```sql
ALTER TABLE <table> DROP CONSTRAINT <table>_<column>_check;
ALTER TABLE <table> ADD CONSTRAINT <table>_<column>_check CHECK (<new_expr>);
```

The two-step emission is required because the SQL emitter for
`SetCheck(Some(expr))` synthesizes the same constraint name regardless of
whether one already exists; without the explicit DROP, the second ALTER would
collide on the constraint name slot. The pair is symmetric, easy to read in
audit logs, and reuses both existing emitter arms unchanged.

**Online safety.** All three lifecycle operations classify as `OnlineSafe` on
empty tables. On populated tables, the ADD case routes through the two-phase
constraint validation default (per the `Two-phase constraint validation
default (Phase 7.5)` decision row): `ADD CONSTRAINT … NOT VALID` followed by
a separate `VALIDATE CONSTRAINT` step under `ShareUpdateExclusiveLock`. DROP
is always catalog-only.

**Family extensibility.** The same `field_type_check` projection helper is
designed to grow with future type families: temporal year bounds (djogi#187),
Decimal precision (djogi#188), and HeerId / RanjId structural validation
(djogi#189) all plug into the same match without reshaping the helper
signature. See `decisions.md` "Type-derived CHECK projection (Phase 8.5 v3
Cluster 2)" for the contract.

**Currently shipped vs deferred.** The projection contract, the AMEND DROP+ADD
fix, and `IntoFilterValue for u64` ship under djogi#186. Two pieces are gated
on djogi#190:

  * The `rust_type_to_sql` arms for `i8 / u8 / u16 / u32 / u64` are gated on
    djogi#190 (per-field bind/decode shims in the macro emitter) —
    `tokio_postgres::ToSql` binds `i8` as `"char"` and `u32` as `OID`, and has
    no impl for `u8 / u16 / u64` at all.
  * The projection wiring at `migrate/projection.rs::project_column` that
    invokes `field_type_check` to populate `ColumnSchema.check` is also gated
    on djogi#190. The descriptor today exposes only `f.sql_type` (a typed
    `FieldSqlType` such as `BigInt`); it does not carry the Rust source type,
    so the projection cannot tell `i64 → BIGINT` from `u32 → BIGINT`. Wiring
    the helper unconditionally would project a `>= 0 AND <= 4294967295` CHECK
    onto every BIGINT column, including the framework's own HeerId-backed
    `id` columns whose values exceed `u32::MAX` from day one. djogi#190 must
    add a per-field "rust source type" discriminator on `FieldDescriptor`
    alongside the bind/decode shims, and at that point `project_column` flips
    on the call to `field_type_check` (gated on the discriminator) and the
    contract surface lights up for `i8 / u8 / u16 / u32 / u64`.

The helper, the differ AMEND DROP+ADD lifecycle, and the
`FieldSqlType::NumericPrecision { precision, scale }` variant are all
unit-tested today against synthetic descriptor / `ColumnSchema` shapes; only
the projection wiring sits dormant so the contract layer cannot regress
production tables before #190 closes the source-type gap.

### 10.7 Ledger and Locking

The migration ledger table is `djogi_schema_migrations`.

```sql
CREATE TABLE IF NOT EXISTS djogi_schema_migrations (
    id                    BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    version               TEXT        NOT NULL UNIQUE,
    description           TEXT        NOT NULL DEFAULT '',
    checksum_up           VARCHAR(68) NOT NULL,
    checksum_down         VARCHAR(68),
    execution_mode        TEXT        NOT NULL DEFAULT 'transactional'
                                  CHECK (execution_mode IN ('transactional', 'non_transactional')),
    status                TEXT        NOT NULL DEFAULT 'pending'
                                  CHECK (status IN ('pending', 'applied', 'baseline', 'faked', 'rolled_back', 'failed')),
    applied_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    applied_by            TEXT        NOT NULL DEFAULT current_user,
    execution_time_ms     BIGINT      NOT NULL DEFAULT 0,
    out_of_order_flag     BOOLEAN     NOT NULL DEFAULT false,
    applied_steps_count   INTEGER     NOT NULL DEFAULT 0,
    total_steps           INTEGER,
    partial_apply_note    TEXT,
    run_id                BIGINT      NOT NULL,
    snapshot_version      TEXT        NOT NULL,
    app_label             TEXT        NOT NULL DEFAULT ''
);

CREATE INDEX djogi_schema_migrations_status_idx
    ON djogi_schema_migrations (version)
    WHERE status != 'applied';

CREATE INDEX djogi_schema_migrations_run_id_idx
    ON djogi_schema_migrations (run_id);
```

Checksum contract:

- algorithm: SHA-256
- input: SQL file bytes after BOM stripping and line-ending normalization to `\n`
- storage format: `V1:` + 64 lowercase hex chars
- `checksum_up` and `checksum_down` hash SQL content only, not filename/version/description

Advisory lock contract:

- key: `0x444A4F474D494752`
- decimal: `4994068948568834898`
- acquired before reading the pending set
- session-scoped
- released in a finally-equivalent cleanup path
- lock namespace is per database target

### 10.8 Apply, Rollback, Repair, and Adoption

Canonical CLI surface:

```bash
# Registered in djogi-cli today (Phase 7 T6 / T7 / T8)
djogi migrations compose
djogi migrations status
djogi migrations attune
djogi migrations attune <target>
djogi migrations attune <target> --apply
djogi migrations attune <target> --apply --record
djogi migrations attune --record-ledger --apply
djogi migrations attune --squash --from V<ts> --apply
djogi db reset --yes
djogi db seed
djogi db seed --database crud_log
djogi docs

# Phase-7-deferred — library APIs ship today; CLI dispatch lands in a follow-up
# task (see Codex round-1 A-4 / A-5 closeout in T8). The runtime entry points
# (`apply_plan`, `rollback_plan`, `verify`, `repair_*`, `baseline_plan`) are
# all public and exercised by the integration test suite; the gap is the
# config / snapshot / plan / ledger plumbing the CLI dispatch needs around
# them. Adopters who need these flows ahead of the CLI registration can wire
# the library APIs directly today.
# djogi migrations apply
# djogi migrations apply --fake 0005_add_vehicle_horsepower
# djogi migrations rollback
# djogi migrations verify
# djogi migrations repair
# djogi migrations repair --rebuild-snapshot
# djogi migrations baseline 0001_initial
```

`migrations attune` is the migration-history state-management command.

Contract:

- it attunes local on-disk migration history to a specified local or remote Git target
- it may fetch if needed to resolve that target
- it does not mutate the database unless `--apply` is explicitly passed
- it does not update the parent repo's recorded submodule pointer unless `--record` is explicitly passed or a command mode clearly implies recording, such as `--squash`
- `--squash` is a dev-history operation for creating a new squashed migration set
- `--squash` is hard-gated behind a four-condition safety contract: localhost database URL resolution, `Djogi.toml::profile != "production"`, `Djogi.toml::[database].dev_mode = true`, and `DJOGI_ENV` env var NOT case-insensitive `"production"`. All four gates are enforced before any I/O so a refusal produces zero side effects on disk or in the ledger
- `--squash` must refuse when the migration history is already treated as shared staging/production history
- publishing a squashed history requires the explicit `--publish` flag and a configured remote (the CLI verb is `--publish`, not `--push`, per the OQ-04 ruling in `docs/spec/decisions.md`)

`attune` does not reconcile seed runs. Seeds live at a separate ledger (`djogi_seed_runs`) and follow a separate lifecycle: `djogi db seed --database <name>` discovers `seeds/<name>/*.sql`, applies each one once, and records the result keyed by file name + checksum. The two ledgers do not share any data flow — schema migrations are reproducible, idempotent operations on shape; seeds are operator-authored data that may not survive `db reset` and intentionally lives outside the schema-snapshot contract. Per Codex umbrella (PARTIAL): `attune` is scoped to `djogi_schema_migrations` reconciliation; an operator who wants to inspect or re-run seeds runs `djogi db seed` directly. The asymmetry is by design — conflating the two ledgers would muddle the snapshot invariants the migration runner owes T5 / T7.

Apply semantics:

- acquire advisory lock
- verify checksums before execution
- write a pre-execution ledger row with `status = 'pending'`
- execute SQL
- transition row to `applied` on success
- update snapshot once after the full successful apply run

Transactional migrations:

- ledger pre-write and DDL share one transaction
- failure rolls back both

Non-transactional migrations:

- pending row commits before DDL begins
- each committed step increments `applied_steps_count`
- partial failure writes `.migration_failure.json`
- further apply/rollback work is blocked until `repair`

Rollback semantics:

- rollback order is reverse ledger insertion order (`id` descending), not version-string order
- rollback restores schema shape only; deleted data is not recoverable through the migration system

Adoption flows:

- `baseline <version>` records all migrations up to a floor as present without running SQL
- `apply --fake <version>` records a specific migration as present without running SQL
- both set `applied_at = now()`
- both are explicit operator actions

### 10.9 Verification and Out-of-Order Policy

Verification is a first-class concern of the engine; the
`migrations verify` CLI subcommand is **deferred post-Phase-7** (per
§7.4 of this spec). The library entry point is available today as
[`djogi::migrate::verify`](../../djogi/src/migrate/verify.rs), and
adopters can drive it directly or via the `djogi::migrate::repair_*`
helpers until the CLI dispatch lands.

The verification path compares snapshot/ledger expectations against
the live database catalog and is used for:

- baseline validation
- out-of-band DDL discovery
- snapshot rebuild workflows
- post-failure recovery validation

Out-of-order policy:

- local/dev: allowed by default, but always recorded and warned
- CI/prod: rejected by default
- override: explicit `--allow-out-of-order`
- ledger always records `out_of_order_flag = true` when applicable

Out-of-order state is never silent.

### 10.10 Destructive Classification and Composite Scope

Djogi classifies generated operations into two buckets:

- `unexecutableSteps` — generation blocked unless explicitly confirmed
- `warnings` — generation proceeds with explicit warning comments

For `0.1.0`, `DROP TABLE` and `DROP COLUMN` require explicit destructive confirmation.

Composite boundary:

- supported:
  - composite unique constraints
  - composite unique indexes
  - composite non-unique indexes
- not supported as a first-class ORM/migration contract:
  - composite primary keys

For supported composite unique/index cases, Djogi must preserve declared column order in both diffs and generated SQL.

### 10.10a Primary-Key Flip Support Matrix

The `pk_flip` family that lives under `djogi::migrate::pk_flip`
ships migration playbooks for the four built-in asc↔desc primary-key
pairs only:

| Before                  | After                   | Supported? | Mechanism                  |
|-------------------------|-------------------------|------------|----------------------------|
| `HeerId`                | `HeerIdRecencyBiased`   | yes        | `pk_flip` Heer family      |
| `HeerIdRecencyBiased`   | `HeerId`                | yes        | `pk_flip` Heer family      |
| `RanjId`                | `RanjIdRecencyBiased`   | yes        | `pk_flip` Ranj family      |
| `RanjIdRecencyBiased`   | `RanjId`                | yes        | `pk_flip` Ranj family      |
| any custom newtype A    | any custom newtype B    | rejected   | hand-written migration     |
| any built-in            | any custom newtype      | rejected   | hand-written migration     |
| any custom newtype      | any built-in            | rejected   | hand-written migration     |
| `HeerId` ↔ `Serial`     | (any cross-family pair) | rejected   | hand-written migration     |
| anything                | composite reshape       | rejected   | hand-written migration     |

A "custom newtype" is a primary-key type declared through the
`djogi::primary_key! { ... }` macro — see the
[primary keys spec](./primary-keys.md#35b-custom-primary-key-types-djogiprimary_key)
for the macro grammar.

**Why custom-PK shape flips are rejected in v0.1.0.** Every custom
newtype carries an adopter-defined inner SQL type (`BIGINT`, `UUID`,
adopter-installed domain types, …) and an adopter-defined `default_sql`
generator. A safe migration between two custom shapes must answer
three questions the framework cannot derive on its own:

1. **The value-preserving cast.** `BIGINT → UUID` has no implicit cast
   in Postgres; a wrong `USING` clause silently truncates row IDs.
2. **The FK cascade strategy.** Every column that holds a foreign key
   to the migrating table must be re-typed in lockstep. The asc↔desc
   flips can do this safely because the inner SQL type does not change;
   custom shape changes can.
3. **The DEFAULT generator's bulk-allocation contract.** Pre-existing
   pre-allocated IDs (Pattern 2 / Pattern 3 from the
   [primary keys spec §3.5](./primary-keys.md#35-id-generation-patterns))
   may need re-issuing under the new generator.

When the differ encounters any transition involving a custom PK kind
on either side, it emits a typed `SchemaOperation::Unsupported` whose
`reason` field names the affected table, classifies the bucket
(`custom-to-custom`, `built-in-to-custom`, `custom-to-built-in`), and
surfaces the inner `type_name` / `sql_type` for the custom side(s).
`compose` then refuses to lower the delta and surfaces this string
verbatim through `ComposeError::UnsupportedDelta` so adopters see the
exact reason without grepping the differ.

Tracking issue: [djogi#165](https://github.com/TarunvirBains/djogi/issues/165).

The model-level declaration grammar (`#[model(indexes(...))]`), the unique-constraint-vs-unique-index lowering rules, the deterministic name format, and the `concurrently = true` operator contract — including the apply-time advisory warning — are specified in the [indexing spec](./indexing.md). This chapter covers only how `IndexSpec` feeds the differ and the generated DDL; the contract itself lives there.
