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

Snapshot format — the complete `AppliedSchema` top-level shape (keys alphabetical, as emitted by the runtime):

```json
{
  "djogi_version": "0.1.0",
  "enums": {},
  "format_version": "1",
  "generated_at": "2026-04-25T00:00:00Z",
  "indexes": [],
  "models": {},
  "registered_apps": [""]
}
```

Field semantics:

| Field | Type | Role |
|---|---|---|
| `djogi_version` | string | Djogi version that wrote this snapshot. Informational; the loader does not gate on it. Useful for forensics when a snapshot looks wrong. |
| `enums` | object | Postgres `CREATE TYPE` entries for `#[derive(DjogiEnum)]` registrations, keyed by the SQL type name. Empty object when no enums are registered. |
| `format_version` | **string** | Snapshot format version — always `"1"` today. The value is a JSON string, not a number. |
| `generated_at` | string | RFC 3339 UTC timestamp of the last successful apply that wrote this snapshot. Informational only. |
| `indexes` | array | Flat index list, sorted by `(table, name)` for determinism. Each entry carries its owning `table` name. |
| `models` | object | Per-table snapshots for this `(target, app)` bucket, keyed by Postgres table name. |
| `registered_apps` | array | App labels registered when this snapshot was written. The synthetic global bucket (no `#[model(app = ...)]`) is represented by `""`. Sorted alphabetically. |

**Snapshot parsing contract.**

- All snapshot structs carry `#[serde(deny_unknown_fields)]`.
- The loader uses a two-stage parse. In the first stage it peeks `format_version` using a permissive `serde_json::Value` parse — so a version mismatch surfaces as an actionable `"snapshot format version '...' is not supported by this Djogi"` error rather than a confusing `unknown field` or `missing field` error from a field the older shape doesn't recognise. In the second stage it runs the full strict structural deserialize.
- A leading UTF-8 BOM (`EF BB BF`) is stripped on load; it is never emitted on write.
- **Bump policy.** Additive fields marked `#[serde(default)]` do not require a `format_version` bump — older snapshots load cleanly because the default supplies the missing value, and no unrecognised field name appears on the wire. Renames, removals, and variant reshapes require a bump because there is no defaulting bridge across those changes.

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

Execution mode (transactional vs non-transactional) is **not** carried by a comment directive in the generated SQL file. The composed migration plan tags each segment with `SegmentKind::Transactional` or `SegmentKind::NonTransactional`, and the runner dispatches by segment kind. Operations that force a non-transactional segment — `CREATE INDEX CONCURRENTLY` is the canonical example — originate from `IndexSchema::requires_out_of_transaction` (and equivalents on other operations) on the descriptor; the segment planner reads that flag at compose time and tags the resulting segment accordingly.

Segment-metadata contract:

- segment kind is set at compose time from operation-level metadata (`IndexSchema::requires_out_of_transaction` and equivalents), not by inspecting the SQL file text
- the runner reads `SegmentKind` from the composed plan and chooses the transactional or non-transactional execution path from that tag
- the generated SQL file is a human-review artifact replayed under the kind the composed plan carries — it is **not** the source of truth for execution mode
- external tooling that round-trips or re-emits a composed migration must preserve segment metadata; a file-level comment directive is not honoured by the current runner

A future revision may add a `-- djogi:no-transaction` (or equivalent) directive parser/generator so the SQL file itself encodes the execution mode end-to-end and external tooling can rely on the file alone. That directive is **not** part of the current contract — today's contract is segment-metadata-driven, and adopters and tooling must treat the composed plan as authoritative.

Destructive generated SQL uses `-- DJOGI WARNING:` comments in the UP file so code review sees the risk in the forward path, not only in rollback text.

### 10.5 Multi-Database Scope

Djogi's migration architecture explicitly accounts for multiple database targets over time.

Examples include:

- the primary application database
- the CRUD log database
- the event log database
- future service-owned databases

The execution contract remains strict:

- one migration plan applies to one `(database, app)` bucket at a time
- each database target has its own ledger (shared across all apps in that target)
- each database target has its own snapshot set (finer-grained, per `(target, app)`)
- advisory locking is per bucket — each `(database, app)` bucket has its own advisory-lock key, so independent buckets within the same target do not contend (key derivation in §10.7)
- repair, baseline, verify, and apply are bucket-scoped
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

### 10.6.1 Type-Derived CHECK Projection (djogi#186 / #187 / #188 / #190)

**Status — temporal, integer, and Decimal arms all live.** The contract layer
ships under djogi#186: the `field_type_check` helper, the differ AMEND
DROP+ADD lifecycle, the `FieldSqlType::NumericPrecision { precision, scale }`
variant, and the `IntoFilterValue for u64` shim. djogi#187 wires the temporal
family (`time::Date` / `time::OffsetDateTime`) end-to-end through
`project_column`. djogi#190 wires the integer family
(`i8 / u8 / u16 / u32 / u64`) with:

- `rust_type_to_sql` macro arms for all five narrow / unsigned types.
- `rust_source_type: Option<RustSourceType>` discriminator on
  `FieldDescriptor` — distinguishes `i8 → SMALLINT` from `i16 → SMALLINT`,
  `u32 → BIGINT` from `i64 → BIGINT`, etc.
- Bind shims in the `#[model]` macro emitter (crud.rs): each field type
  widens to the matching `tokio_postgres::ToSql`-compatible wire type.
- Decode shims with bounds-checked narrowing (from_row.rs, from_joined_row.rs).
- `field_type_check` dispatch gated on the discriminator so only the five
  widened types receive a range CHECK; direct-mapped types keep `check: None`.

djogi#188 wires the Decimal arm with a discriminator-only path —
`RustSourceType::Decimal` rides on the same descriptor slot but signals only
the projection layer (no bind/decode shim, since `rust_decimal::Decimal`
implements `postgres_types::ToSql for NUMERIC` and the matching `FromSql`
natively). The arm emits a structural CHECK enforcing the 96-bit mantissa /
scale-≤-28 representable range; bare `NUMERIC` stays the column type so
adopter-supplied values keep their full precision (no rounding).

djogi#105 adds the adopter-supplied `#[field(check = "<sql>")]` attribute.
The string is treated as a raw SQL escape — djogi does not parse or
sanitize the expression beyond rejecting empty / whitespace-only literals
at macro-parse time. When a column carries both a type-derived CHECK and
an adopter CHECK, the projection layer combines them with logical `AND`
into a single constraint slot (`<table>_<column>_check`).

See "Currently shipped vs deferred" below for the full piece-by-piece state.

Under the contract, the differ projects a table-level `CHECK` constraint for
every column whose Rust source type widens to a Postgres column type. The
projection is type-driven and runs at descriptor → snapshot lowering time,
so the resulting CHECK serializes into `schema_snapshot.json` and survives
every round-trip.

**Mapping table.** Each Rust source type widens to the smallest signed Postgres
integer that fits its full value range; `u64` widens to bare `NUMERIC`
(no precision/scale) because `u64::MAX > i64::MAX`. Bare `NUMERIC` is used
rather than `NUMERIC(20, 0)` because a precision/scale column silently rounds
fractional inputs before the CHECK fires — making the CHECK ineffective
against fractional raw INSERTs. The `col = trunc(col)` clause in the CHECK
rejects any fractional value at the DB level. The temporal types use a
one-sided upper-bound CHECK matching `time::Date::MAX_YEAR` /
`time::OffsetDateTime::MAX`; the `TIMESTAMPTZ` form uses an explicit UTC
literal (`+00`) so the bound is timezone-invariant (a plain `TIMESTAMP '...'`
literal against a `TIMESTAMPTZ` column is interpreted in the session timezone,
widening the effective UTC upper bound by the session UTC offset).

| Rust source              | Postgres column | Type-derived CHECK expression                                                                          | Status   |
|--------------------------|-----------------|--------------------------------------------------------------------------------------------------------|----------|
| `time::Date`             | `DATE`          | `<col> <= DATE '9999-12-31'`                                                                           | **Live** |
| `time::OffsetDateTime`   | `TIMESTAMPTZ`   | `<col> <= TIMESTAMPTZ '9999-12-31 23:59:59.999999+00'`                                                 | **Live** |
| `i8`                     | `SMALLINT`      | `<col> >= -128 AND <col> <= 127`                                                                       | **Live** |
| `u8`                     | `SMALLINT`      | `<col> >= 0 AND <col> <= 255`                                                                          | **Live** |
| `u16`                    | `INTEGER`       | `<col> >= 0 AND <col> <= 65535`                                                                        | **Live** |
| `u32`                    | `BIGINT`        | `<col> >= 0 AND <col> <= 4294967295`                                                                   | **Live** |
| `u64`                    | `NUMERIC`       | `<col> >= 0 AND <col> <= 18446744073709551615 AND <col> = trunc(<col>)`                                | **Live** |
| `rust_decimal::Decimal`  | `NUMERIC`       | `scale(<col>) <= 28 AND abs(<col>) * power(10::numeric, scale(<col>)) <= 79228162514264337593543950335` | **Live** |

Identity-mapped widths (`i16`, `i32`, `i64`, `bool`, `String`, `f32`, `f64`,
...) project no CHECK because the column type already covers their full range.
FK columns inherit the parent PK's identity-width type, so they project no
CHECK either. The `f32 → REAL` mapping is locked at the identity row: see
`docs/spec/decisions.md` "Type-derived CHECK projection" for why a future
widening to `DOUBLE PRECISION` would re-open the bug class djogi#185 exists
to close.

**Constraint naming.** Each projected CHECK becomes a table-level constraint
named `<table>_<column>_check`, deterministic from `(table, column)`. The
naming function lives at `djogi/src/migrate/sql.rs::check_constraint_name` and
truncates to Postgres' 63-byte identifier limit by appending an 8-char hex
digest to a 54-byte stem.

**IR shape — `SetCheck { from, to }`.** Each `ColumnChange::SetCheck` entry
carries both the prior CHECK expression (`from`) and the target CHECK
expression (`to`). The SQL emitter uses `from` for the down-side rollback
so every CHECK transition has a fully recoverable rollback path with no
"prior expression not recoverable" comment placeholder. (The earlier
`SetCheck(Option<String>)` design dropped the prior expression on the
floor and made the down side of any DROP arm structurally lossy — visible
whenever a type migration on a checked column was rolled back.)

**ADD lifecycle.** Descriptor evolves from a column with no CHECK (e.g. `i64`)
to a column whose Rust source projects a CHECK (e.g. `u32`). The differ at
`migrate/diff.rs::emit_alter_column` emits
`ColumnChange::SetCheck { from: None, to: Some(expr) }` which the SQL emitter
renders as:

```sql
-- up
ALTER TABLE <table> ADD CONSTRAINT <table>_<column>_check CHECK (<expr>);

-- down (rollback)
ALTER TABLE <table> DROP CONSTRAINT <table>_<column>_check;
```

**DROP lifecycle.** Descriptor evolves from a column whose Rust source projects
a CHECK (e.g. `u32`) to a column with no CHECK (e.g. `i64`). The differ emits
`ColumnChange::SetCheck { from: Some(prior), to: None }` which renders as:

```sql
-- up
ALTER TABLE <table> DROP CONSTRAINT <table>_<column>_check;

-- down (rollback — restores the prior expression losslessly)
ALTER TABLE <table> ADD CONSTRAINT <table>_<column>_check CHECK (<prior>);
```

**AMEND lifecycle.** Descriptor evolves from a column with one CHECK to a
column with a different CHECK (e.g. `u16` → `u32`, or any `#[field(check)]`
expression edit). The differ detects the AMEND case explicitly and emits two
`ColumnChange` entries in order:

```rust
ColumnChange::SetCheck { from: Some(old), to: None }
ColumnChange::SetCheck { from: None, to: Some(new) }
```

The composed up file runs the two steps forward (drop old, add new). The
composed down file walks them in reverse (per
`compose::compose_down_text`), giving the operator: drop the new CHECK,
then re-add the old CHECK. The combined SQL pair is:

```sql
-- up
ALTER TABLE <table> DROP CONSTRAINT <table>_<column>_check;
ALTER TABLE <table> ADD CONSTRAINT <table>_<column>_check CHECK (<new_expr>);

-- down (rollback — restores the original CHECK losslessly)
ALTER TABLE <table> DROP CONSTRAINT <table>_<column>_check;
ALTER TABLE <table> ADD CONSTRAINT <table>_<column>_check CHECK (<old_expr>);
```

The two-step emission is required because the SQL emitter for an `ADD`
step synthesizes the same constraint name regardless of whether one
already exists; without the explicit DROP, the second ALTER would collide
on the constraint name slot. The pair is symmetric, easy to read in audit
logs, and reuses both existing emitter arms unchanged.

**Online safety.** All three lifecycle operations classify as `OnlineSafe` on
empty tables. On populated tables, the v0.1.0-alpha apply path
(`djogi::migrate::apply_plan` consuming SQL from `migrate/sql.rs`) emits a
single-statement `ALTER TABLE … ADD CONSTRAINT … CHECK (…)`, which acquires
`AccessExclusiveLock` for the duration of validation. The two-phase
constraint validation default (per the `Two-phase constraint validation
default (Phase 7.5)` decision row) — `ADD CONSTRAINT … NOT VALID` followed
by a separate `VALIDATE CONSTRAINT` step under `ShareUpdateExclusiveLock` —
is the planned Phase 7.5 live-plan rollout shape. The pattern catalogue at
`live_migrate::patterns::TwoPhaseValidate` covers the foreign-key arm today;
the CHECK / NOT NULL arms are queued behind the live-plan runner surface
(`djogi live run` / `djogi live finalize`), which remains stubbed / deferred
in v0.1.0 per the `Live-plan dashboard deferral (Phase 10 / Maahi)` decision
row. DROP is always catalog-only.

**Inline CHECK on CREATE TABLE.** The SQL emitter renders the projected
CHECK inline on the column definition using the
`<col> <type> ... CONSTRAINT <name> CHECK (<expr>)` form rather than the
unnamed `CHECK (<expr>)` form. The explicit `CONSTRAINT` keyword makes
the constraint name deterministic — Postgres's auto-naming for unnamed
inline CHECKs is `{table}_check` / `{table}_check1` / ..., which would
diverge from the differ's ALTER TABLE DROP CONSTRAINT path
(`{table}_{column}_check`). With the explicit name on inline emission,
the CREATE TABLE and ALTER TABLE pathways reach the same constraint slot
and the differ's drop / amend lifecycle works against both.

**Family extensibility.** The same `field_type_check` projection helper is
designed to grow with future type families. djogi#188 (Decimal precision) is
now live alongside djogi#187 (temporal) and djogi#190 (integer); HeerId /
RanjId structural validation (djogi#189) plugs into the same match without
reshaping the helper signature when that work lands. See `decisions.md`
"Type-derived CHECK projection (Phase 8.5 v3 Cluster 2)" for the contract.

**Currently shipped.** The full projection contract is now live:

  * djogi#186 — contract scaffolding: `field_type_check` helper,
    AMEND DROP+ADD lifecycle, `FieldSqlType::NumericPrecision`,
    `IntoFilterValue for u64`.
  * djogi#187 — temporal wiring: `time::Date` / `time::OffsetDateTime`
    year upper-bound CHECKs reach `ColumnSchema.check`,
    `schema_snapshot.json`, and generated migration SQL.
  * djogi#188 — Decimal wiring: `rust_decimal::Decimal` model fields
    project a structural CHECK enforcing the 96-bit mantissa / scale-≤-28
    representable range. The column stays bare `NUMERIC`; the CHECK is
    discriminator-driven via `RustSourceType::Decimal` so adopter
    `Decimal` fields are distinguished from `u64 → NUMERIC` columns at
    projection time without a bind / decode shim.
  * djogi#190 — integer widening: `i8 / u8 / u16 / u32 / u64` model
    fields compile, bind, decode, and receive type-derived CHECKs.
    `RustSourceType` discriminator on `FieldDescriptor` gates dispatch
    so direct-mapped types (`i16 / i32 / i64`) never receive spurious CHECKs.

The integer bound strings are pinned in `migrate/sql.rs::alter_column_set_check_for_*`
tests (`i8`, `u32`, `u64` shapes) and in the new unit tests inside
`migrate/projection.rs` (positive round-trip CHECK assertions for all five types).
The temporal arms are covered by `field_type_check_for_date_emits_year_upper_bound`
and `field_type_check_for_timestamptz_emits_year_upper_bound` (unit tests),
`project_column_emits_year_check_for_non_fk_date_column` /
`..._timestamptz_column` (projection wiring tests), and the
`tests/internal/phase8_5_c2_187_temporal_year_check.rs` integration test.
The integer widening end-to-end coverage lives in
`tests/internal/phase8_5_c2_190_integer_widening.rs`. The Decimal arm and
the adopter `#[field(check)]` AND-merge contract are covered by
`field_type_check_for_decimal_numeric_emits_structural_bounds`,
`project_column_emits_decimal_structural_check_for_non_fk_numeric_column`,
the `combine_check_expressions_*` projection unit tests, and
`tests/internal/phase8_5_c2_105_188_check_decimal.rs`.

### 10.6.2 Adopter `#[field(check = "<sql>")]` (djogi#105)

Adopters can declare arbitrary CHECK constraints on any model field via
`#[field(check = "<sql expression>")]`. The expression is emitted verbatim
into the column's CHECK constraint inside both inline `CREATE TABLE` form
(`CONSTRAINT <table>_<column>_check CHECK (<expr>)`) and ALTER-TABLE form
(`ALTER TABLE … ADD CONSTRAINT … CHECK (<expr>)`).

```rust
#[derive(djogi::Model)]
#[model(table = "animals")]
pub struct Animal {
    pub name: String,
    #[field(check = "weight_kg > 0")]
    pub weight_kg: f64,
}
```

**Raw SQL escape.** The expression is treated identically to a raw SQL
fragment. djogi performs **no parsing, no sanitization, and no semantic
validation** beyond rejecting empty / whitespace-only literals at parse
time. Adopters are responsible for:

- The expression's syntactic correctness against the column's Postgres type.
- Its idempotency — CHECK predicates must be `IMMUTABLE` to be acceptable
  to Postgres (no `now()`, no volatile function calls, no references to
  other tables / rows).
- Identifier handling — column names referenced inside the expression
  must be the Postgres column name. If the field name happens to collide
  with a reserved keyword, the expression author quotes the identifier
  manually (e.g. `"\"order\" >= 0"`).

The same `unsafe`-style cultural posture from
`docs/spec/raw-sql-escape-hatches.md` applies — every callsite is
reviewable as raw SQL.

**Combination with type-derived CHECKs.** When a column also receives a
type-derived CHECK (e.g. an adopter `u32` field with
`#[field(check = "port > 0")]`), the projection layer combines the two
with logical `AND` into a single constraint slot:

```sql
CONSTRAINT "<table>_<column>_check" CHECK (
    (<type-derived-expr>) AND (<adopter-expr>)
)
```

Both clauses must pass for an INSERT / UPDATE to land. The single
constraint slot keeps the ADD / DROP / AMEND lifecycle in the differ
unchanged — `<table>_<column>_check` carries exactly one CHECK per
column, whether it came from the framework, the adopter, or both.

**Lifecycle.** The combined CHECK rides the same ADD / DROP / AMEND
machinery as the type-derived CHECK (see §10.6.1 above). Adding or
changing an adopter `#[field(check)]` expression triggers the differ's
AMEND path — `SetCheck { from: Some(old), to: None }` followed by
`SetCheck { from: None, to: Some(<new combined>) }` — which produces
a `DROP CONSTRAINT … ; ADD CONSTRAINT …` SQL pair whose rollback
restores the prior combined expression losslessly.

**FK columns.** Adopter CHECKs are honoured on FK columns even though
the type-derived CHECK is suppressed on FKs (FK column types inherit
from the parent's PK, which is always identity-width). The adopter may
want a domain invariant on the FK column itself — e.g.
`#[field(check = "owner_id > 0")]` to reject the HeerId sentinel value
zero on a non-null FK column. The projection emits the adopter
expression directly with no `AND`-merge wrapping in this case.

**Validation rules.** `FieldAttrs::parse` rejects empty and
whitespace-only literals with a span-precise diagnostic. Every other
string is accepted at parse time; SQL-level rejection happens at
migration apply (Postgres returns a `42601` syntax error or `42703`
column-not-found error on bad expressions, with the full constraint
name in the error message so operators can locate the offending field).

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

- scope: per migration bucket, where bucket = `(database, app)`; independent buckets hash to distinct keys and do not contend on a single global literal key
- key derivation: `SHA-256("djogi:advisory_lock:" || database || "\0" || app)`, with the first 8 digest bytes interpreted as a big-endian signed 64-bit integer (Postgres `bigint`)
- prefix: the `djogi:advisory_lock:` byte prefix scopes the keyspace so adopter-side advisory locks that hash arbitrary identifiers cannot collide with Djogi's keys
- acquired before reading the pending set for the bucket
- session-scoped — the design intent is that the holder pins a single session (e.g. a dedicated non-pooled `tokio_postgres::Client`) for the full `apply` / `rollback` / `repair` window so pool reuse cannot silently swap the holder; the current runner acquires the lock through a supplied pool-backed `DjogiContext` and per-operation checkouts are not yet pinned to one session for the migration window — pinning is a release-gate hardening item (track ahead of v0.1.0 publish), not a current runtime guarantee
- released in a finally-equivalent cleanup path

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
# deferred CLI sketch: djogi migrations apply
# deferred CLI sketch: djogi migrations apply --fake 0005_add_vehicle_horsepower
# deferred CLI sketch: djogi migrations rollback
# deferred CLI sketch: djogi migrations verify
# deferred CLI sketch: djogi migrations repair
# deferred CLI sketch: djogi migrations repair --rebuild-snapshot
# deferred CLI sketch: djogi migrations baseline 0001_initial
```

`migrations attune` is the migration-history state-management command.

Contract:

- it attunes local on-disk migration history to a specified local or remote Git target
- it may fetch if needed to resolve that target
- `--apply` commits ledger/disk reconciliation changes, but it does not execute migration SQL or apply schema DDL
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

---

### 10.10b DDL metadata attributes

The following operational DDL metadata features are surfaced as model or field
attributes and lowered by `djogi migrations compose`.

| Feature | Attribute | Lowering | Tracking |
|---|---|---|---|
| `COMMENT ON TABLE` | `#[model(table_comment = "...")]` | `COMMENT ON TABLE <t> IS '...'`; `IS NULL` when cleared | [#217](https://github.com/TarunvirBains/djogi/issues/217) |
| `COMMENT ON COLUMN` | `#[field(comment = "...")]` | `COMMENT ON COLUMN <t>.<c> IS '...'`; `IS NULL` when cleared | [#217](https://github.com/TarunvirBains/djogi/issues/217) |
| Per-table storage parameters (`fillfactor`, `autovacuum_*`, …) | `#[model(storage_params = "key=val, ...")]` | `ALTER TABLE <t> SET (key=val, ...)`; prior keys are reset when changed or cleared | [#218](https://github.com/TarunvirBains/djogi/issues/218) |
| `CREATE TABLE … TABLESPACE <name>` | `#[model(tablespace = "...")]` | `ALTER TABLE <t> SET TABLESPACE <name>` after table creation; clearing lowers to `pg_default` | [#219](https://github.com/TarunvirBains/djogi/issues/219) |

The remaining DDL metadata gaps are:

| Feature | Planned attribute | Workaround today | Tracking |
|---|---|---|---|
| `ALTER COLUMN … TYPE … USING <expr>` | `#[field(type_change_using = "...")]` | Hand-edit the `ALTER COLUMN TYPE` statement in the composed SQL to add `USING (<expr>)` | [#220](https://github.com/TarunvirBains/djogi/issues/220) |
| Generated column expression changes (PG 15+) | differ-automatic (verify status) | Confirm the differ emits `DROP EXPRESSION` + `SET EXPRESSION AS` rather than column-recreate; hand-write if not | [#221](https://github.com/TarunvirBains/djogi/issues/221) |

All six pieces are tracked under the [#172](https://github.com/TarunvirBains/djogi/issues/172)
umbrella. The closing condition for the umbrella is all six pieces across five sub-issues (#217–#221) landed and
`docs/spec/migrations.md` + `docs/guide/models.md` reflecting the final attribute contracts.
