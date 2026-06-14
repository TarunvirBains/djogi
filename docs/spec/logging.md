> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

## 9. Automated Audit & Event Logging — Two Parallel Databases

Djogi separates all logging concerns across two purpose-built databases, each isolated from application data. The framework maintains three concurrent connection pools at startup: application (`url`), CRUD audit (`crud_log_url`), and event/observability (`event_log_url`).

| Database | Config Key | Purpose | Typical Retention |
|---|---|---|---|
| `myapp` | `url` | Application data | Permanent |
| `myapp_crud_logs` | `crud_log_url` | Structural CRUD audit trail | Long-term / compliance |
| `myapp_event_logs` | `event_log_url` | Request, crash, and debug events | Short-to-medium term |

The separation means CRUD logs from two years ago live untouched regardless of how aggressively event logs from last week are pruned.

Maintainability matters as much as capability here. Djogi should make the common case obvious: maintainers pick a logging profile and move on. The framework may expose advanced sink-by-sink overrides, but the primary UX is profile-first rather than a matrix of buffering, retry, startup, and failure knobs.

### 9.0 Logging Profiles

Djogi ships three named logging profiles:

| Profile | CRUD Log Behavior | Event Log Behavior | Intended Use |
|---|---|---|---|
| `light` | best-effort | best-effort | local development, low-friction setups |
| `balanced` | durable bounded retry | best-effort | default production posture |
| `strict_audit` | fail-closed | best-effort by default | compliance-sensitive audit trails |

Profiles exist to keep the feature usable. Most adopters should never need to reason about queue depth, retry mode, or outage policy directly.

Advanced per-sink overrides may exist, but they are escape hatches layered under the profile system rather than the recommended entry point.

### 9.1 CRUD Log Database — Structural Audit Trail

Every model with CRUD logging enabled automatically provisions a mirror `_logs` table in the CRUD Log Database (e.g. `Vehicle` → `vehicle_logs` in `myapp_crud_logs`).

After a successful application write, Djogi emits a CRUD audit record to this database according to the configured logging profile. "Asynchronous" here means the app write and the log write do not rely on distributed SQL transactions spanning both databases. Djogi may buffer or dispatch the audit write outside the foreground request path for `light` and `balanced` profiles, but it must not misrepresent that as cross-database atomic commit.

The application database remains the system of record. When maintainers choose stricter audit semantics, Djogi enforces them by rejecting or surfacing the app operation according to the CRUD delivery policy; it does not promise two-phase commit.

Enabling:
```toml
# Djogi.toml
[database]
url          = "postgres://localhost/myapp"
crud_log_url = "postgres://localhost/myapp_crud_logs"
event_log_url = "postgres://localhost/myapp_event_logs"

[logging]
profile = "balanced"

[features]
crud_log = false   # global default — opt in per model or globally
```
```rust
// Per model
#[model(table = "vehicles", crud_log = true)]
#[derive(Debug, Clone)]
pub struct Vehicle { ... }

// Opt out a specific model when globally enabled
#[model(table = "internal_tokens", crud_log = false)]
#[derive(Debug, Clone)]
pub struct InternalToken { ... }
```
The mirror log table schema (auto-provisioned per model):
```sql
CREATE TABLE vehicle_logs (
    id          BIGINT PRIMARY KEY DEFAULT heerid_next_desc(),
    record_id   BIGINT NOT NULL,
    event       TEXT NOT NULL CHECK (event IN ('created', 'updated', 'deleted')),
    changes     JSONB,               -- array of FieldChange — null for created/deleted
    snapshot    JSONB,               -- full record snapshot for created/deleted
    actor       TEXT,                -- optional — who made the change
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_vehicle_logs_record ON vehicle_logs (record_id);
CREATE INDEX idx_vehicle_logs_occurred_at ON vehicle_logs (occurred_at);
```
JSON-aware diffing — changes to both regular model fields and nested `Jsonb<T>` subfields are captured with full dot-notation path precision. Unknown fields that changed between load and save are also captured.
```rust
// CrudEvent::Updated written to vehicle_logs:
// changes: [
//   { path: "make",                    before: "Toyota", after: "Lexus"  },
//   { path: "engine.horsepower",       before: 450,      after: 500      },
//   { path: "engine.turbo.boost_psi",  before: 15.0,     after: 18.5    },
// ]
```
Redaction in diffs (Phase 6.5 — see [Protected Data Metadata & Field Codecs](./protected-data.md)): fields annotated with `#[field(sensitive)]` or `#[field(redact_in(logs))]` have their before/after values replaced with a masked marker in `changes` and `snapshot`. Fields declared with `#[field(codec = "...")]` are captured in their codec-encoded form — the CRUD log never sees plaintext. This keeps the audit trail complete (which field changed, when, by whom) without the log database becoming a secondary plaintext store of sensitive data.

CRUD delivery semantics by profile:

- `light` — best-effort. Audit enqueue or write failure is surfaced in health/metrics output, but the application write still succeeds.
- `balanced` — durable bounded retry. Djogi persists a clear operational warning when the CRUD sink is degraded and retries within a bounded buffer policy, but does not block the app write by default.
- `strict_audit` — fail-closed. If Djogi cannot satisfy the configured CRUD audit requirement, the originating application write fails rather than committing without its required audit record.

Only CRUD audit may be promoted to fail-closed in the default profiles. Event logging remains operational telemetry, not a precondition for application correctness.

Actor attribution:
```rust
car.save_with_actor(&mut ctx, "user:8312847293").await?;
// or via request-context hook — all saves in a handler attributed automatically
```
Querying from the shell:
```rhai
// Full history for a record — uses the crud_log pool automatically
let history = VehicleLog::objects()
    .filter(|f| f.record_id.eq(car.id))
    .order_by(|f| f.occurred_at.desc())
    .fetch_all();

pp(history);

// Changes to a specific nested field
let changes = VehicleLog::objects()
    .filter(|f| f.event.eq("updated"))
    .json_path_changed("engine.horsepower")
    .fetch_all();
```

Startup and outage behavior:

- app database startup is always mandatory
- missing or unreachable log databases are handled according to the active profile
- `light` may start with either log sink unavailable
- `balanced` should start with degraded-sink warnings and clear health output when a log sink is unavailable
- `strict_audit` must refuse startup when CRUD logging is required but the CRUD log sink is unavailable

Djogi should expose sink health through metrics, CLI/operator output, and tracing so maintainers can tell the difference between "logging enabled" and "logging healthy".

### 9.2 Event Log Database — Observability & Tracing

All behavioral, observability, and system events are routed to the Event Log Database. This covers request logs, handler lifecycle, crash reports, and debug traces — anything that describes what the system did, not what data changed.

- **Tracing integration:** Built on `tracing` + `tracing-subscriber`. A background Layer routes structured spans and events into `_djogi_events` in the Event Log Database
- **Severity routing:** `DEBUG`/`INFO` spans go to the events table; `WARN`/`ERROR`/`CRITICAL` additionally fan out to Sentry, OpenTelemetry, or Datadog via standard subscriber layering — no custom adapters needed
- **Access pattern:** Semi-structured `JSONB` payloads, queried constantly during active debugging, naturally suited to shorter retention than CRUD logs

Event-log delivery is intentionally simpler than CRUD audit:

- default behavior is best-effort under every built-in profile
- event-log sink failure must never silently claim delivery it did not achieve
- event-log sink failure must never retroactively invalidate a committed app write
- warnings, counters, and dropped-event metrics should make loss visible to operators

Teams that truly need durable event ingestion should compose Djogi's tracing output with external observability infrastructure rather than forcing the ORM's event sink to become a distributed transaction coordinator.

### 9.2.1 Migration Ownership

Djogi owns the schema lifecycle for all three databases:

- app-data schema migrations apply to the app database
- CRUD mirror-table migrations apply to the CRUD log database
- event-log schema migrations apply to the event-log database

The operator workflow should stay unified even though the databases are separate. Maintainers should not need three unrelated migration systems just because logging is isolated.

Where a migration affects both app and log schemas, Djogi should generate and apply the required work per target database with explicit labeling of which database each step touches.

Migration execution remains target-scoped at the library and configuration level. The shipped `djogi migrations` CLI currently exposes `compose`, `status`, `attune`, and `verify` without a `--target` selector; target-specific app/log database flows are represented through configured migration buckets and direct library entry points until a dedicated CLI target selector is registered. `verify` is the exception that already routes per target: it walks every configured migration bucket and connects each bucket to the pool for its database, so a single `djogi migrations verify` checks the app, CRUD-log, and event-log schemas in one run.

Each target owns its own ledger, snapshot, and advisory-lock scope. Djogi may later coordinate ordered multi-target workflows, but it does not claim distributed atomic migration across the app, CRUD-log, and event-log databases. `apply` ships as `djogi migrations apply`, `verify` ships as `djogi migrations verify`, `repair` ships as `djogi migrations repair`, `baseline` ships as `djogi migrations baseline`, and `rollback` ships as `djogi migrations rollback`.

### 9.3 Log Database Retention

> Note: this section covers retention of the log databases themselves. For application-data lifecycle (purge / anonymize / archive of rows in the app DB), see [Data Lifecycle & Governance](./data-lifecycle.md).

```bash
# Wipe app DB only — both log databases untouched
djogi db reset

# Log database wipe flags are planned, not registered in the shipped CLI.
# Until they land, reset log databases through operator-owned maintenance
# scripts rather than `djogi db reset`.
```
`db reset` guards (`dev_mode`, localhost URL, `DJOGI_ENV`) apply to the shipped app-database reset path; log-database wipe flows must preserve the same guard posture when implemented.

`db reset` remains app-first UX:

- `djogi db reset` only resets the app database
- explicit flags are required before Djogi touches either logging database
- the CLI output should name each database being reset so operators do not infer a single-cluster wipe from one command

Retention policies for CRUD logs and event logs are independent. CRUD retention is normally long-lived and compliance-sensitive; event-log retention is shorter and operational. Djogi should not force them into the same purge cadence.
