> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

## 13. The Shell

### 13.1 Invocation
```bash
cargo djogi shell
```
Starts a Rhai REPL with all registered models pre-loaded, a live database connection, and a persistent command history file.
### 13.2 Async Strategy

The shell holds a dedicated single-threaded `tokio` runtime. Every terminal method (`fetch_all`, `fetch_one`, `first`, `count`, `save`, `delete`, `create`, `get`, `fetch` on FK) wraps its async implementation in `runtime.block_on(...)` internally.
No `.await` anywhere in the shell. Blocking is a feature — the shell is for interactive exploration, not production throughput.
### 13.3 Error Handling

Errors print a clean one-liner in the shell and never unwind the session. Local variables, open transactions, and history all survive. The full traceback is written to `.djogi_shell_errors/` so it's available when needed without cluttering the prompt.
```
djogi> let car = Vehicle::get(99999)
Error: record not found (vehicles where id = 99999)
  → traceback saved to .djogi_shell_errors/2025-03-26T10-42-11_001.log

djogi> let car = Vehicle::get(42)
djogi> car.gas_fill = 90
djogi> car.save()
djogi>
```
Errors inside a transaction do not auto-rollback — that is always the developer's explicit choice. Where Postgres itself aborts the transaction (e.g. constraint violation), the shell notes it and waits:
```
djogi (txn)> car.owner_id = 99999
djogi (txn)> car.save()
Error: foreign key violation — owner_id 99999 does not exist in owners
  → traceback saved to .djogi_shell_errors/2025-03-26T10-42-33_002.log
Note: Postgres has aborted this transaction. Call rollback() to clear it.
djogi (txn)>
```
Error log format:
```
# .djogi_shell_errors/2025-03-26T10-42-33_002.log

Timestamp:   2025-03-26T10:42:33Z
Session:     started 2025-03-26T10:40:11Z
Transaction: open (began 2025-03-26T10:41:05Z)

Error:       foreign key violation
Message:     insert or update on table "vehicles" violates foreign key constraint
             "vehicles_owner_id_fkey"
Detail:      Key (owner_id)=(99999) is not present in table "owners"

Rhai stack:
  at save() [built-in]
  at line 3, col 1 in shell session

SQL attempted:
  UPDATE vehicles SET owner_id = $1, updated_at = $2 WHERE id = $3
  params: [99999, 2025-03-26T10:42:33Z, 42]
```
Full stack traces are enabled in all log files by default. Pass `--verbose` at shell startup to also print them inline for framework debugging.
Log files are auto-purged on shell startup based on `error_log_retention` (default: `1y`). Manual clear: `.clear_errors`.
### 13.4 Transactions in the Shell

The shell exposes explicit transaction control. Open transactions are clearly indicated in the prompt.
Starting a transaction prompts for an optional timeout — not enforced by the framework, but offered as a reminder. The `transaction_timeout_default` config value pre-fills the prompt; the developer can accept it, change it, or clear it entirely:
```
djogi> begin()
Transaction timeout [default: 30m, or enter duration e.g. 1h, or leave blank for none]: _
Note: uncommitted work will be lost if the shell exits or loses connection.
djogi (txn)>
```
Timeout is advisory — it's a Postgres `SET LOCAL statement_timeout` on the connection, not a framework timer. The developer who is mid-thought and steps away is not penalized unless they opted into a timeout. Defaulting to none would be fine; the config default just pre-fills a sensible suggestion.
```
djogi> begin()
Transaction timeout [default: 30m]: 
djogi (txn)> let car = Vehicle::get(42)
djogi (txn)> car.gas_fill = 0
djogi (txn)> car.save()
djogi (txn)> pp(Vehicle::get(42))    // inspect safely inside the transaction
djogi (txn)> commit()
djogi>

// Or change your mind
djogi (txn)> rollback()
djogi>
```
Power loss / process crash: Postgres handles this correctly at the protocol level. When the shell process dies — cleanly or not — the TCP connection drops and Postgres automatically rolls back any open transaction. No partial commits, no manual cleanup required. The developer reconnects via `cargo djogi shell` and the database is exactly as it was before `begin()`.
Closing the shell with an open transaction always triggers an explicit `ROLLBACK` before exit — never a silent commit.
Savepoints for complex sessions:
```rhai
begin();
savepoint("checkpoint");
// risky work
rollback_to("checkpoint");
// try something else
commit();
```
### 13.5 Shell Capabilities
```rhai
// Query — no .await
let cars = Vehicle::objects()
    .filter_struct(VehicleFilter::new().gas_fill(Gte(69)).active(Eq(true)))
    .fetch_all();

pp(cars);                            // ASCII table
print(cars[0].make);

// FK traversal
let owner = cars[0].owner_id.fetch();

// M2M
let groups = person.groups();
person.add_to_group(group, #{ role: "admin" });
person.remove_from_group(group);

let members = group.members();

// Mutate
let car = Vehicle::get(42);
car.gas_fill = 90;
car.save();

// Create
let car = Vehicle::create(#{ make: "Ford", model_name: "F-150", gas_fill: 60, active: true });

// Delete
car.delete();

// Raw SQL
let rows = sql("SELECT make, COUNT(*) FROM vehicles GROUP BY make");
pp(rows);
```
### 13.6 Shell Utilities

| Helper | Description |
|---|---|
| `pp(value)` | ASCII table (collections) or key-value (single model) |
| `sql("...")` | Raw SQL — returns array of dynamic maps |
| `begin()` | Start a transaction, prompts for optional timeout |
| `commit()` | Commit open transaction |
| `rollback()` | Rollback open transaction |
| `savepoint("name")` | Create a savepoint |
| `rollback_to("name")` | Rollback to savepoint |
| `.export name` | Save current session history to `scripts/name.rhai` |
| `.export name --from bookmark` | Save from a named bookmark position |
| `.bookmark name` | Bookmark current history position |
| `.import name` | Run `scripts/name.rhai` inside the current session |
| `reload()` | Re-initialize model bindings mid-session |
| `.clear_errors` | Delete all logs in `.djogi_shell_errors/` |

### 13.7 Session Import / Export
Shell sessions can be saved as named Rhai scripts and replayed later or shared with teammates.
Export writes the current session's meaningful history to `scripts/`:
```
djogi> .export analysis_q3_vehicles
Saved to scripts/analysis_q3_vehicles.rhai
```
Raw navigation (up-arrow corrections, typos) is filtered out. The developer can open the file, clean it up, and commit it.
Bookmark a mid-session position to export from a specific point:
```
djogi> .bookmark before_delete
djogi> .export backfill_owners --from before_delete
Saved to scripts/backfill_owners.rhai
```
Import / run inside the REPL:
```
djogi> .import analysis_q3_vehicles
Running scripts/analysis_q3_vehicles.rhai...
djogi>
```
Headless run without entering the REPL:
```bash
cargo djogi shell --run scripts/analysis_q3_vehicles.rhai
```
Scripts run in the full shell environment with access to all models. They are useful beyond history replay — lightweight data analysis, one-off backfills, or team-shared query libraries.
Gitignore convention:
```
.djogi_history            # gitignored — personal, noisy
.djogi_shell_errors/      # gitignored — full tracebacks, retained for 1y by default
scripts/                  # committed — curated, shareable
seeds.rhai                # committed — project seed data
```
### 13.8 Seed Scripts
`seeds.rhai` at project root runs in the full shell environment:
```bash
cargo djogi db seed
```
Uses the same model API the developer already knows.
---
