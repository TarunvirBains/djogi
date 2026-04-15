> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

## 9. Automated Audit & Event Logging — Two Parallel Databases

Djogi separates all logging concerns across two purpose-built databases, each isolated from application data. The framework maintains three concurrent connection pools at startup: application (`url`), CRUD audit (`crud_log_url`), and event/observability (`event_log_url`).

| Database | Config Key | Purpose | Typical Retention |
|---|---|---|---|
| `myapp` | `url` | Application data | Permanent |
| `myapp_crud_logs` | `crud_log_url` | Structural CRUD audit trail | Long-term / compliance |
| `myapp_event_logs` | `event_log_url` | Request, crash, and debug events | Short-to-medium term |

The separation means CRUD logs from two years ago live untouched regardless of how aggressively event logs from last week are pruned.

### 9.1 CRUD Log Database — Structural Audit Trail

Every model with CRUD logging enabled automatically provisions a mirror `_logs` table in the CRUD Log Database (e.g. `Vehicle` → `vehicle_logs` in `myapp_crud_logs`). After any successful `INSERT`, `UPDATE`, or `DELETE`, the framework asynchronously writes the before/after snapshot and a `JSONB` diff to this database.
Enabling:
```toml
# Djogi.toml
[database]
url          = "postgres://localhost/myapp"
crud_log_url = "postgres://localhost/myapp_crud_logs"
event_log_url = "postgres://localhost/myapp_event_logs"

[features]
crud_log = false   # global default — opt in per model or globally
```
```rust
// Per model
#[derive(Model)]
#[model(table = "vehicles", crud_log = true)]
pub struct Vehicle { ... }

// Opt out a specific model when globally enabled
#[derive(Model)]
#[model(table = "internal_tokens", crud_log = false)]
pub struct InternalToken { ... }
```
The mirror log table schema (auto-provisioned per model):
```sql
CREATE TABLE vehicle_logs (
    id          BIGINT PRIMARY KEY DEFAULT generate_id(),
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
Actor attribution:
```rust
car.save_with_actor(&pool, "user:8312847293").await?;
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
### 9.2 Event Log Database — Observability & Tracing

All behavioral, observability, and system events are routed to the Event Log Database. This covers request logs, handler lifecycle, crash reports, and debug traces — anything that describes what the system did, not what data changed.

- **Tracing integration:** Built on `tracing` + `tracing-subscriber`. A background Layer routes structured spans and events into `_djogi_events` in the Event Log Database
- **Severity routing:** `DEBUG`/`INFO` spans go to the events table; `WARN`/`ERROR`/`CRITICAL` additionally fan out to Sentry, OpenTelemetry, or Datadog via standard subscriber layering — no custom adapters needed
- **Access pattern:** Semi-structured `JSONB` payloads, queried constantly during active debugging, naturally suited to shorter retention than CRUD logs

### 9.3 Log Lifecycle
```bash
# Wipe app DB only — both log databases untouched
cargo djogi db reset

# Wipe app DB and CRUD log DB — event logs retained
cargo djogi db reset --wipe-crud-logs

# Wipe all three databases
cargo djogi db reset --wipe-all-logs
```
`db reset` guards (`dev_mode`, localhost URL, `DJOGI_ENV`) apply to all variants.
