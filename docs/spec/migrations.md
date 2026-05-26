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

If a non-transactional migration fails partway through, the snapshot remains unchanged. The runner then makes a best-effort failure-path ledger update that records the partial state in `djogi_schema_migrations`: the row is marked `failed`, `applied_steps_count` records how many steps committed before the failure, and `partial_apply_note` names the failing step and error. Because that bookkeeping write happens on the failure path, operators must treat it as best-effort recovery metadata rather than a stronger guarantee; if the update itself fails, the row may remain pending or partially updated, but the snapshot still does not move forward.

The same `version` stays occupied in the ledger until repair resolves the row in place, so another apply of the same version still collides on the unique `version` constraint. Use `djogi migrations status` to inspect the row, then resolve it with `repair_partial_apply` or, when the row is still resumable, `repair_resume_partial_apply`.

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
now live alongside djogi#187 (temporal) and djogi#190 (integer). HeerId /
RanjId structural validation (djogi#189) ships as a parallel
**opt-in** projection (`strict_id_check_expr`) rather than a new arm
inside `field_type_check` because the CHECK depends on the column's
HeerRanjID **semantic family** — the parent `PkType` for the framework
`id` column and the FK target's `PkType` (via `type_to_pk_family`) for
relation columns — not on `FieldSqlType` or the resolved SQL type
string. The semantic-family dispatch keeps Custom PKs whose inner
SQL_TYPE coincidentally matches BIGINT / UUID from being coerced into
the HeerRanjID family by SQL-carrier collision. See §10.6.3 for that
surface. See `decisions.md` "Type-derived CHECK projection (Phase 8.5
v3 Cluster 2)" for the type-derived contract and "HeerId / RanjId
structural CHECK (djogi#189)" for the opt-in surface.

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

### 10.6.3 Opt-in HeerId / RanjId Structural CHECK (djogi#189)

`HeerId` (BIGINT) and `RanjId` (UUID) columns carry **no** automatic
structural CHECK by default. The column type alone admits any value
inside its representable range — a negative BIGINT (bit 63 = 1) or a
UUIDv4 / UUIDv7 / nil-UUID written into a UUID column survives the
INSERT and only surfaces as a typed-decode failure when later read back
through `HeerId::from_i64` / `RanjId::from_uuid`. The single bad row
poisons subsequent typed reads, matching the same external-writer hole
the temporal and integer families address (§10.6.1).

The fix is an **opt-in** structural CHECK that runs at the DB layer to
reject structurally-malformed IDs at INSERT time. The opt-in is exposed
at two granularities:

```rust
// Model-wide — every applicable column on the model gets the CHECK.
#[derive(djogi::Model)]
#[model(table = "vehicles", strict_ids)]
pub struct Vehicle {
    pub owner_id: ForeignKey<Owner>,    // structural CHECK applied
    pub plate_id: ::djogi::RanjId,      // structural CHECK applied
    pub name: String,                   // no CHECK — not HeerId/RanjId
}

// Field-level — single column scope.
#[derive(djogi::Model)]
#[model(table = "vehicles")]
pub struct Vehicle {
    #[field(strict_id_check)]
    pub external_owner: ::djogi::HeerId,  // structural CHECK applied
    pub other_id: ::djogi::HeerId,         // no CHECK — opt-in is per field
}
```

**Default-off is the contract.** Pre-#189 schemas continue to round-trip
identically after #189 lands. The opt-in is documented as a perf vs
safety trade — adopters who never accept externally-generated IDs into
HeerId / RanjId columns can keep the framework's existing zero-overhead
default; adopters who do (BI tools, sister applications, raw SQL
migrations, third-party imports) opt in to harden the surface.

**CHECK shapes.** The projection layer reads the field descriptor's
`strict_id_check: bool` flag plus the column's HeerRanjID **semantic
family** — derived from the parent `PkType` for the framework `id`
column, from the FK target's `PkType` (via `type_to_pk_family`) for
relation columns, and from `f.sql_type` for explicit `#[field(strict_id_check)]`
on bare HeerId / RanjId user scalars — and dispatches:

| HeerRanjID semantic family                                                              | Projected CHECK                                                                                                            |
|-----------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------|
| `HeerId` (`PkType::HeerId` / `PkType::HeerIdDesc`, BIGINT carrier)                      | `<col> >= 0`                                                                                                               |
| `RanjId` (`PkType::RanjId` / `PkType::RanjIdDesc`, UUID carrier)                        | `pg_catalog.substring(<col>::text, 15, 1) = '8' AND pg_catalog.substring(<col>::text, 20, 1) IN ('8','9','a','b')`         |
| None — `PkType::Serial`, `PkType::Custom`, `PkType::Composite`, `PkType::None`          | no CHECK (silently skipped — see "FK propagation" and "Custom PK semantics" below)                                         |

The HeerId family CHECK enforces `HeerId::from_i64`'s only structural
invariant (`bit 63 = 0`, i.e. non-negative i64). The 41 timestamp + 9
node + 13 sequence bits saturate the remaining 63 bits without a
reserved-bit slot to enforce. Verified against
`~/projects/HeeRanjID/heeranjid/src/heer.rs` at the time this attribute
shipped.

The RanjId family CHECK enforces UUIDv8 + RFC 4122 variant — the two
structural invariants `RanjId::from_uuid` rejects. The check extracts
position 15 of the canonical 8-4-4-4-12 text form (version hex digit,
must be `'8'`) and position 20 (variant high nibble, must be one of
`'8'`, `'9'`, `'a'`, `'b'`, covering the `10xx` RFC 4122 variant
pattern). The substring calls use `pg_catalog.substring` to prevent
search-path hijacking; Postgres's `pg_get_constraintdef` renders this
back as the unqualified `"substring"(...)` form. Postgres canonicalises
UUID text output to lowercase hex, so the variant set covers every
encoding the column will ever hold. Verified against
`~/projects/HeeRanjID/heeranjid/src/ranj.rs` (`from_uuid`'s version /
variant guards) and `ranj_desc.rs` (the `RANJ_FLIP_MASK` preserves bits
76-79 and 62-63, so ascending and descending RanjId variants share the
same structural CHECK shape).

**Custom PK semantics.** A `PkType::Custom { sql_type: "BIGINT" / "UUID",
.. }` PK (e.g. an adopter Snowflake-style or UUIDv4 application ID)
shares the SQL carrier with HeerId / RanjId but is NOT a HeerRanjID
identifier — its bit layout is defined by the adopter's `PrimaryKey`
impl, not by HeeRanjID. The family-based dispatch correctly maps Custom
PKs to "no CHECK" regardless of the inner SQL_TYPE — both the framework
`id` column on a Custom-PK model AND every FK column targeting a
Custom-PK model are skipped under `#[model(strict_ids)]`. The earlier
SQL-type-only dispatch that this design supersedes would have emitted
`col >= 0` against Custom BIGINT PKs (constraining the adopter's value
domain without consent) and the UUIDv8 + RFC 4122 CHECK against Custom
UUID PKs (rejecting every valid UUIDv4 the adopter inserts). Adopters
whose Custom PK genuinely shares HeerRanjID's bit layout and who want
the structural CHECK should declare it explicitly via
`#[field(check = "<predicate>")]` — the typed-and-explicit path is
preferred over inferring the family from a coincidental SQL-carrier
match.

**Per-row cost.** The BIGINT CHECK is a single comparison (`<1 µs`).
The UUID CHECK casts the column to text and runs two `substring` calls
(~1–3 µs). Both are opt-in because automatic emission would re-shape
the default model surface and break adopters who legitimately accept
externally-generated IDs into these columns.

**FK propagation.** When `#[model(strict_ids)]` fires, every FK / O2O
column on the model gets the descriptor flag set, because the macro
cannot inspect the FK target's PK family at parse time. The projection
resolves the FK target's HeerRanjID semantic family via the
`type_to_pk_family` lookup (the parallel of `pk_sql_type_text` /
`type_to_pk_sql` for SQL carrier substitution) and dispatches per the
table above — FKs to HeerId / RanjId targets get the matching CHECK,
FKs to Serial-PK / Composite-PK / None-PK targets silently skip, and
FKs to **Custom-PK** targets ALSO silently skip regardless of whether
the Custom PK's inner SQL_TYPE is `"BIGINT"` or `"UUID"`. The
family-based dispatch is the correct filter: a Custom PK is not a
HeerRanjID carrier, even when its SQL type collides with one. Adopters
who declare `#[model(strict_ids)]` on a model with heterogeneous FK
targets get strict checks on the HeerId / RanjId references and the
existing default-off behaviour on Serial / Custom / Composite references
— no special handling required at the adopter site.

**Validation at the attribute surface.** `#[field(strict_id_check)]` is
parse-time-validated against the field's declared Rust type. Acceptable
types are: bare HeerId / HeerIdDesc / HeerIdRecencyBiased / RanjId /
RanjIdDesc / RanjIdRecencyBiased (in any path form — bare, `djogi::*`,
`djogi::types::*`), `ForeignKey<T>`, `OneToOneField<T>`, and the
schema-transparent `Option<…>` / `Tracked<…>` wraps around any of
the above (the validation routes through `unwrap_schema_type`, which
strips both wrappers). Other types (`String`, `i64`, `i32`, etc.) are
rejected with a span-precise diagnostic pointing at the offending
attribute — silent dropping of an explicit opt-in is a poor UX.
Model-wide `#[model(strict_ids)]` does **not** trip this check; it is
a bulk opt-in where silently skipping non-applicable fields is the
intended behaviour.

**Combination with other CHECKs.** The strict-ID CHECK rides the same
single-constraint-slot infrastructure as the type-derived and adopter
CHECKs (§10.6.1, §10.6.2). When a column accumulates more than one
CHECK source (e.g. `#[field(strict_id_check, check = "owner_id <> 0")]`
on an FK), the projection layer ANDs the strict-ID expression with the
adopter expression into a single `<table>_<column>_check` constraint.
Both clauses must pass for an INSERT to land. Mutually exclusive in
practice with the type-derived CHECKs from §10.6.1 — HeerId / RanjId
fields carry `rust_source_type: None`, so the integer / Decimal /
temporal arms of `field_type_check` are inert on them.

**Lifecycle.** ADD / DROP / AMEND all ride the existing `SetCheck { from,
to }` lifecycle from §10.6.1. Toggling `#[model(strict_ids)]` (or
`#[field(strict_id_check)]`) on or off triggers the same `DROP
CONSTRAINT ... ADD CONSTRAINT ...` shape Postgres handles for every
other CHECK transition. Rollback through `compose_down_text` reinstates
the prior expression losslessly because the IR carries both the old and
new sides of the transition.

**Migration to Route B (centralized HeeRanjID validator).** The CHECK
expressions projected here live inside djogi and track HeeRanjID's bit
layout. A future HeeRanjID release will ship `IMMUTABLE PARALLEL SAFE`
Postgres validator functions:

```sql
CREATE FUNCTION heeranjid.is_valid_heerid(BIGINT) RETURNS BOOLEAN ...;
CREATE FUNCTION heeranjid.is_valid_ranjid(UUID)   RETURNS BOOLEAN ...;
```

When those land, djogi will migrate to projecting:

```sql
CHECK (heeranjid.is_valid_heerid(<col>))
CHECK (heeranjid.is_valid_ranjid(<col>))
```

so the validator becomes a single source of truth tracked in the
HeeRanjID repository rather than embedded twice (once in HeerId /
RanjId Rust code, once in djogi's projection layer). The opt-in
attribute surface stays unchanged across the migration — `#[model(strict_ids)]`
and `#[field(strict_id_check)]` keep their semantics and adopter
code does not move. See "HeerId / RanjId structural CHECK (djogi#189)"
in `docs/spec/decisions.md` for the route A / route B comparison.

**Test coverage.** The family → CHECK projection mapping is pinned by
the `strict_id_check_expr_*` and `strict_id_family_of_pk_*` unit tests
in `migrate/projection.rs`; the end-to-end projection wiring
(default-off, model-wide opt-in, FK propagation, AND-merge with adopter
check, skip on Serial-PK targets, skip on Custom-BIGINT and Custom-UUID
PK targets at both the framework `id` column and at FK columns) is
covered by the `project_column_strict_id_check_*` tests in the same
module. Catalog assertions, round-trip behaviour, OOB rejection of
structurally-malformed IDs (negative BIGINT for HeerId; UUIDv4 / UUIDv7
/ non-RFC4122-variant UUIDs for RanjId), and Custom-PK skip behaviour
for FK targets live in
`tests/internal/phase8_5_c2_189_strict_id_check.rs`. Macro-time type
validation is exercised by
`djogi-macros/tests/compile_fail/phase8_5_c2_189_strict_id_check_wrong_type.rs`
and
`djogi-macros/tests/compile_pass/phase8_5_c2_189_strict_id_check.rs`.

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
# task. The runtime entry points
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

`attune` does not reconcile seed runs. Seeds live at a separate ledger (`djogi_seed_runs`) and follow a separate lifecycle: `djogi db seed --database <name>` discovers `seeds/<name>/*.sql`, applies each one once, and records the result keyed by file name + checksum. The two ledgers do not share any data flow — schema migrations are reproducible, idempotent operations on shape; seeds are operator-authored data that may not survive `db reset` and intentionally lives outside the schema-snapshot contract. `attune` is scoped to `djogi_schema_migrations` reconciliation; an operator who wants to inspect or re-run seeds runs `djogi db seed` directly. The asymmetry is by design — conflating the two ledgers would muddle the snapshot invariants the migration runner owes T5 / T7.

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
- on the failure path, the runner makes a best-effort ledger update that marks the row `failed` and records the step/error details in `partial_apply_note`
- the snapshot remains unchanged on failure
- `djogi migrations status` is the operator-facing view of the row while it is failed or pending
- `repair_partial_apply` resolves the row in place to a terminal status, and `repair_resume_partial_apply` resumes a still-resumable failure from `applied_steps_count + 1`
- further apply work for the same `version` still collides on the unique constraint until the row is repaired in place

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
| `ALTER COLUMN … TYPE … USING <expr>` | `#[field(type_change_using = "...")]` | one-time directive, see §10.10b.1 below | [#220](https://github.com/TarunvirBains/djogi/issues/220) |
| Generated column expression changes (PG 17+) | differ-automatic | in-place `ALTER COLUMN … SET EXPRESSION AS (<new_expr>)`, see §10.10b.2 below | [#221](https://github.com/TarunvirBains/djogi/issues/221) |

All six pieces are tracked under the [#172](https://github.com/TarunvirBains/djogi/issues/172)
umbrella. The closing condition for the umbrella is all six pieces across five sub-issues (#217–#221) landed and
`docs/spec/migrations.md` + `docs/guide/models.md` reflecting the final attribute contracts.

#### 10.10b.1 `#[field(type_change_using = "<sql expr>")]` (djogi#220)

When the migration differ detects that a column's `sql_type` changed
between the prior snapshot and the freshly-projected schema, the SQL
emitter lowers the transition to:

```sql
ALTER TABLE <t> ALTER COLUMN <c> TYPE <new> USING <c>::<new>;
```

Postgres performs the explicit cast for every pair that has one defined
(widenings like `INTEGER → BIGINT`, `varchar(N) → text`, ...). For
**non-default cast paths** — `TEXT → UUID`, `TEXT → INTEGER`, custom
domain changes, citext flips — the explicit cast `<from>::<to>` is
still defined by Postgres (text→uuid, text→integer, ... are valid
explicit casts), so the framework's emitted statement is accepted at
parse time. The failure mode is per-row: the cast rejects any row whose
value does not parse as a syntactically valid `<to>` literal, surfacing
at apply time as:

```
ERROR: invalid input syntax for type <to>: "<row value>"
```

(or a similar per-pair shape such as `invalid input syntax for
integer`). The framework's emission always carries an explicit
`USING <col>::<new>` clause, so the *bare-USING* error
`column "<c>" cannot be cast automatically to type <new>` (which fires
only when no `USING` is supplied and Postgres falls back to the
assignment cast) does not surface here — it is the row-data shape, not
the cast direction, that causes the apply to fail.

The framework's lowering rule:

1. The adopter annotates the field with
   `#[field(type_change_using = "<sql expr>")]`. The expression is the
   `USING` body — the framework wraps it in `USING (<expr>)` verbatim,
   with no parsing, sanitisation, or escaping.
2. The migration composer emits the typed column-type change as
   `ALTER COLUMN <c> TYPE <new> USING (<expr>);`. The
   adopter-supplied expression fully replaces the default
   `<col>::<new_type>` fallback.
3. The down (rollback) side ALWAYS falls back to the default cast —
   symmetric down-side `USING` expressions are not modelled. The
   rollback path is operator-owned in practice; an adopter whose
   rollback also requires a special cast hand-edits the emitted down
   SQL. **When the forward step uses an adopter `USING (<expr>)`,
   the emitter additionally attaches a `LossyRollbackWarning` of kind
   `CustomCast`** so `LossyRollbackPolicy::Refuse` (the default)
   engages and requires explicit operator opt-in before rolling
   back — see [`crate::migrate::sql::LossyRollbackKind::CustomCast`].
4. When `#[field(type_change_using = "...")]` is absent and the differ
   detects a known-incompatible cast pair (`TEXT ↔ UUID`,
   `TEXT ↔ INTEGER/SMALLINT/BIGINT`, `UUID ↔ integer family`), the
   emitter prepends a `-- WARNING:` SQL comment to the migration that
   names the corrective attribute. The comment is a soft signal — the
   default cast still emits and Postgres still rejects the migration at
   apply time (per-row `invalid input syntax for type ...`). The
   warning helps adopters discover the corrective attribute before the
   apply-time error surfaces in CI.

**Raw SQL escape.** The expression is treated identically to a raw SQL
fragment — djogi performs no parsing, sanitisation, or validation
beyond rejecting empty / whitespace-only strings at macro-parse time.
**A wrong USING expression can silently corrupt or truncate column
data** (see the §custom-PK-shape-flips note above on truncation risk).
The same "raw SQL is djogi's `unsafe`" cultural posture from
[`raw-sql-escape-hatches.md`](./raw-sql-escape-hatches.md) applies —
every `#[field(type_change_using = "...")]` callsite should be
reviewable as raw SQL. Test the migration in a non-production
environment before applying it to data.

**Lifetime — one-time directive.** The attribute is consulted only at
the moment the differ emits a `ChangeType` for this column. The
`ColumnSchema.type_change_using` slot is `#[serde(skip)]` and excluded
from the manual `PartialEq` impl, so:

- Leaving the attribute on the field after the migration applies
  produces no phantom diff — the next compose run sees the same
  `sql_type` on both sides and emits nothing.
- The snapshot on disk never carries the value.
- Adopters are encouraged (but not required) to remove the attribute
  from source after the migration applies. The framework does not
  enforce removal.

**Live-plan limitation.** `#[field(type_change_using = "...")]` forces
the migration to the **offline-apply path**. The live-plan
shadow-column pattern at
[`crate::live_migrate::patterns::replacement_column`] can only emit a
default SQL cast (`SET <shadow> = <col>::<to>`) in its chunked
backfill — it cannot replicate an adopter-supplied USING expression
in a per-row `SET`. The classifier
([`crate::live_migrate::classify::classify_column_change`]) therefore
routes `ColumnChange::ChangeType { using: Some(_), .. }` to
`OnlineSafetyClassification::OfflineOnly` so the dispatcher never
sees a non-default-cast change. The dispatcher and both relevant
pattern emitters
([`crate::live_migrate::patterns::dispatch_pattern`],
[`crate::live_migrate::patterns::replacement_column`], and
[`crate::live_migrate::patterns::codec_transition`]) carry a
defense-in-depth `CannotEmit` refusal for the same case. Adopters who
need a non-default cast supply `type_change_using` and apply the
migration via the offline `migrations apply` path; the live-plan path
is reserved for cast pairs Postgres handles with its built-in
explicit cast and no row-data hazards.

Macro-level parse-time validation rejects `#[field(type_change_using = "")]`
and whitespace-only literals with a span-precise diagnostic pointing
at the offending string. The validator also rejects the following
attribute combinations:

- `type_change_using` paired with `#[field(generated = "...")]` — a
  stored generated column derives its storage type from the
  expression. `ALTER COLUMN TYPE` on a generated column re-evaluates
  the generation expression with the new storage type, and Postgres'
  semantics for USING on a stored generated column are surprising at
  best. Hand-edit the migration if a stored generated column needs
  to flip storage type.
- `type_change_using` on a `ForeignKey<T>` or `OneToOneField<T>`
  field — FK type changes happen via PK flips on the parent model
  (Phase 7 PK-flip orchestration), not as direct column type changes
  on the child side. An adopter USING here cannot drive the typed
  PK-flip apparatus.

Field-level `#[field(identity)]` does not exist as a user-facing
attribute — the projection assigns
`identity: Some(IdentityKindSchema::ByDefault)` to the auto-injected
`id` column on `pk = Serial` models, and that field is not
user-modifiable, so the combination cannot arise at macro parse time.

See `djogi-macros/tests/compile_fail/phase8_5_c4_220_type_change_using_*`
for the pinned diagnostic shapes.

#### 10.10b.2 Generated column expression changes (djogi#221)

When a `#[field(generated = "<sql expr>")]` column changes its
expression between two compose runs, the migration differ surfaces the
transition as `ColumnChange::SetGenerated { from: Some(prev), to: Some(next) }`.
The SQL emitter lowers this to the Postgres 17+ in-place form:

```sql
ALTER TABLE <t> ALTER COLUMN <c> SET EXPRESSION AS (<new_expr>);
```

Postgres rewrites every row under `AccessExclusiveLock` to materialise
the new expression. djogi targets Postgres 18 and later exclusively
(see [`decisions.md`](./decisions.md)), so this form is always
available. The classifier
([`live_migrate::classify`]) routes the operation to `OfflineOnly`
because the row-rewrite lock window matches the offline pattern's
contract — even though the statement is structurally a single
`ALTER COLUMN`, the lock duration scales with row count.

**Rollback shape.** The down side emits the symmetric form
`SET EXPRESSION AS (<prev_expr>);` — the prior expression is restored
in place, no row data is destroyed. Rollback is non-lossy.

**Why a single `SET EXPRESSION AS` and not `DROP EXPRESSION + SET EXPRESSION AS`?**
The two-step form looks like a clean swap, but `SET EXPRESSION AS`
requires the column to still be a generated column — a prior
`DROP EXPRESSION` strips that property and the follow-up
`SET EXPRESSION AS` fails with `column "<c>" is not a generated column`.
Postgres' intended in-place change form is the single statement.

**Other generated-column transitions.**

- `(None, Some(_))` — ADD generation to an existing regular column:
  Postgres has no `ALTER COLUMN ADD GENERATED` for stored expressions.
  The emitter keeps a SQL-comment placeholder documenting the required
  `DROP COLUMN + ADD COLUMN` sequence; the classifier routes the
  operation to `OfflineOnly` and the live-migration runner refuses the
  path entirely.
- `(Some(_), None)` — DROP generation: `ALTER COLUMN <c> DROP EXPRESSION;`
  (PG 13+). The column becomes a regular column with the
  previously-computed values frozen as data. Rollback is structurally
  lossy — restoring the expression in place is impossible
  (`SET EXPRESSION AS` requires a generated column), so the inverse
  requires `DROP COLUMN + ADD COLUMN`, destroying the post-DROP-EXPRESSION
  row data. The emitter marks the rollback `lossy`.
