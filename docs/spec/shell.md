> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

## 13. The Shell

### 13.0 Role of the Shell

The Rhai shell is the **primary surface through which application developers iterate on queries**. It is not an admin REPL or an occasional inspection tool — it is the iteration loop where query patterns are discovered, refined, timed, and rewritten before they are committed back to `.rs` files as `QuerySet` chains. Adopters writing non-trivial query code will spend more time in the shell than in their editor for the duration of that work.

This positioning has consequences:

- **Startup latency is a product feature**, not just a developer convenience. A shell that takes seconds to launch fragments the iteration loop; a shell that launches in well under a second supports the rapid try-revise-retry rhythm queries require. Phase 9 treats startup speed as a hard constraint with measurable budgets, not a "nice to have."
- **Shell ergonomics are first-class.** Persistent history, syntax highlighting, autocomplete on registered model methods, transparent SQL inspection (`EXPLAIN`, last-query echo), and per-call timing all belong in the shell surface — not deferred behind a flag or postponed to "v2."
- **The shell is where other harnesses defer "workshop" affordances.** `lihaaf`'s v0.1 spec (see [`lihaaf-v0.1.md`](./lihaaf-v0.1.md), TBD) explicitly defers interactive workshop mode to Phase 9 on the basis that the Rhai shell is already that workshop for query authors. Building a second interactive surface elsewhere would fragment the iteration story without serving a use case the shell does not already cover.

The closest external analog is Django's `manage.py shell`, but the comparison undersells what Phase 9 ships: Django's shell loads the ORM and stops. Djogi's shell additionally owns the `djqry` authoring loop (§13.9), splits parse from eval for instant syntax-error feedback (§13.10), and is the binary that the dynamic library architecture (§13.11) is sized for.

### 13.1 Invocation
```bash
cargo djogi shell
```
Starts a Rhai REPL with all registered models pre-loaded, a live database connection, and a persistent command history file.

The shell binary is small (~5 MB) and dynamically loads `libdjogi.so` at startup; the dylib coupling is detailed in §13.11.

### 13.2 Async Strategy

The shell holds a dedicated single-threaded `tokio` runtime. Every terminal method (`fetch_all`, `fetch_one`, `first`, `count`, `save`, `delete`, `create`, `get`, `fetch` on FK) wraps its async implementation in `runtime.block_on(...)` internally.
No `.await` anywhere in the shell. Blocking is a feature: the shell exists for query iteration, not for production throughput, and the synchronous surface keeps Rhai authoring linear and copy-pasteable into REPL transcripts that read top-to-bottom without `await`-ladder noise.
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

### 13.9 `djqry` Authoring Loop

The shell is the canonical authoring surface for `djqry` SQL overrides (Phase 9c — see [`implementation-plan.md`](./implementation-plan.md) §9c). The *test → optimize → compile → deploy* cycle never requires leaving the REPL.

```rhai
// Run the macro-query you suspect is suboptimal
let plan = Vehicle::objects()
    .filter_struct(VehicleFilter::new().expired_registration(Eq(true)))
    .prefetch(VehicleRelated::owner_id())
    .fetch_all();

// Capture it as the starting point for an override
djqry.export(last_query, "expired_registrations");
// Writes djqry/expired_registrations.sql with @name, @on, @returns, @binds,
// @replaces, and @signature pre-populated; macro-generated SQL placed in
// the body as the baseline to optimize against.

// Edit the SQL in your editor, then bring it back into the session
djqry.import("expired_registrations");

// Compare macro-query against override side-by-side
djqry.diff("expired_registrations");
// Reports: row-count delta, first-row diff, EXPLAIN cost comparison, timing.
// Acts as the local on-demand analog of CI's `cargo djogi djqry verify`.

// Re-fingerprint after manual re-verification
djqry.sign("expired_registrations");
// Re-computes signature from current @replaces and prompts before overwriting.
```

The shell-side authoring loop is what makes `djqry` viable as a registry rather than a hand-edited file collection: every override starts with the macro-generated SQL djogi would have emitted, every diff is a side-by-side run, and every signature bump is gated on an explicit author confirmation. See `implementation-plan.md` §9c for the on-disk file format, build-time generation pipeline, and runtime dispatch model.

### 13.10 Parse-vs-Eval Split

The shell separates the two phases of Rhai script execution. When the user submits a line:

1. The shell calls `Engine::compile(&input)`. This is a pure parse — no database connection is touched, no script body runs, no terminal method dispatches. Rhai returns either an `AST` or a `ParseError` carrying line and column position plus error kind.
2. If parse fails, the shell prints a one-liner with a caret pointing at the offending column and returns to the prompt. The user never waits on a database call to discover a typo.
3. Only on parse success does the shell call `Engine::eval_ast(&ast)`. Runtime errors (type mismatches, unregistered functions, query failures) flow through the existing error-handling path described in §13.3.

```
djogi> let cars = Vehicle::objects().fitler(|v| v.active().eq(true)).fetch_all()
Parse error: unknown method `fitler` (did you mean `filter`?)
                                        ^^^^^^
djogi>
```

The split is cheap to add (Rhai's `Engine::compile` is built for this use case) and removes a class of frustrating wait-then-fail loops where the user submits a multi-second query only to discover a typo after the database round-trip completes.

**Caveat — Rhai is dynamically typed.** Most semantic errors are runtime errors, not parse errors:

- Calling an unregistered function (`Vehicle::nonexistent()`) is a runtime error.
- Passing the wrong argument type to a registered function is a runtime error.
- Calling a method on the wrong receiver type is a runtime error.

`Engine::compile` catches only true syntax errors — unbalanced braces, malformed expressions, invalid tokens, reserved-word misuse. For the broader class of "this query won't work for reasons the parser can see," Rhai exposes optional strict modes:

- `Engine::set_strict_variables(true)` — undeclared variable references become compile-time errors instead of runtime errors. This catches typos in identifier names without needing a database connection.
- Custom `OnVarFn` resolvers — the shell can hook variable resolution at compile time to verify that referenced model bindings (`Vehicle`, `Owner`, etc.) exist in the registered binding set before `eval_ast` runs.

**Recommendation:** Phase 9 enables `set_strict_variables(true)` by default and registers an `OnVarFn` resolver that validates model-binding identifiers against the `inventory`-collected descriptor set. The cost is a small amount of additional parse-time work on every line; the gain is that mistyped model names (`Vechile::objects()`) surface as parse errors with caret positioning rather than as runtime errors several lines into a script. Function-arity and argument-type checks remain runtime errors — Rhai does not expose a compile-time type checker for dynamic dispatch. Adopters who need stronger pre-execution guarantees should commit the validated query to `.rs` and let the Rust compiler enforce its contracts.

### 13.11 Dynamic Library Coupling

Phase 9 requires `djogi` to be buildable as a Rust dynamic library. The shell binary dynamically loads `libdjogi.so` at startup and dispatches all model, query, descriptor, and runtime calls through the loaded library; queries call into the dylib via Rhai's function-binding layer.

This is a load-bearing architectural choice for Phase 9, and it is shared with `lihaaf` (a separate test harness with its own spec — see [`lihaaf-v0.1.md`](./lihaaf-v0.1.md), TBD). Both surfaces depend on the same property: when djogi is built as a dynamic library, runtime registrations made via `inventory::submit!` inside djogi must be visible to consumers (the shell binary, fixture binaries) that link the dylib. The inventory-on-dylib spike (see [`docs/research/2026-05-10-inventory-on-dylib-spike.md`](../research/2026-05-10-inventory-on-dylib-spike.md), TBD) is currently validating this property; §13.13 describes the contingencies for each spike outcome.

**What the dylib coupling buys.** Be precise about this — easy claims about "faster" do not survive scrutiny:

- **Build iteration speed for shell-side code.** When iterating on shell-crate code (Rhai bindings, REPL UX, autocomplete data) the developer rebuilds the shell crate alone — the canonical `libdjogi.so` is reused. Without the dylib, every shell-crate touch re-links djogi statically into the shell binary and pays djogi's full link cost on every iteration.
- **Plugin architecture via `rhai-dylib`.** Precompiled Rhai modules can ship as `.so` files and link against the canonical `libdjogi.so` rather than each statically baking djogi in (§13.12). As an ecosystem of helper modules grows, this is the difference between linear memory growth (each plugin carries its own copy of djogi) and constant memory growth (every plugin shares the loaded djogi).
- **Smaller shell binary for distribution.** A statically-linked shell carrying the full djogi surface is in the ~100 MB range; a ~5 MB shell binary plus a separately-loaded ~30 MB `libdjogi.so` is materially friendlier to package, ship, and version.
- **Hot-reload potential.** Rebuilding the dylib and dlopen-ing the new version into a running shell session is technically possible. Speculative — not a v0 commitment, but the architecture leaves the door open if adopter pressure surfaces.

**What the dylib coupling does NOT buy.** Critically:

- **Query execution speed.** It does not speed up runtime query execution in any meaningful way. The function-pointer indirection through PLT/GOT adds ~1-2 ns per cross-dylib call, which is lost in the noise next to even the cheapest SQL round-trip. Anyone who claims "the dylib makes queries faster" is wrong; the speed wins are entirely on the build, plugin, memory, and distribution axes.

The honest framing: dylib serves the iteration loop (faster shell rebuilds, smaller binary), the plugin ecosystem (constant-memory module growth), and the distribution story (ship a small binary plus a swappable library). It does not serve the hot path of "query execution latency."

### 13.12 Plugin Loading via `rhai-dylib`

Phase 9 evaluates `rhai-dylib` (https://crates.io/crates/rhai-dylib) as the plugin-loading mechanism. `rhai-dylib` lets Rhai scripts be precompiled to dynamic libraries and dlopen-ed at runtime — Phase 9 considers it for:

- Shipping precompiled Rhai modules (helper libraries, common query patterns, user-defined macros) as `.so` files alongside the shell binary, avoiding per-startup parse costs for large Rhai script libraries
- Letting the user's project ship "Rhai sidecar" modules that get loaded into the shell session — adopters package query helpers as compiled artifacts that other engineers on the team load with one command

**Audit gate.** Before locking djogi's `[lib]` configuration to satisfy `rhai-dylib`'s requirements, Phase 9 includes a 30-minute audit item:

- Read `rhai-dylib`'s documented requirements for symbol visibility, `pub extern "Rust"` annotations, and any other constraints that flow back to djogi's `[lib]` section
- Confirm the crate's documented Rhai-version compatibility window matches the version djogi-shell pins
- Confirm the maintenance status: last release date, open-issue surface, and whether the crate works with the dylib loader djogi is sized for (`libloading` is the most likely candidate)
- Document what was checked and the outcome

`rhai-dylib` is the **planned** plugin mechanism, evaluation pending. It is not committed as a hard Phase 9 dependency until the audit passes. If the audit surfaces material concerns (unmaintained crate, incompatible dylib loader, conflicting symbol-visibility requirements that would constrain djogi's surface), the fallback is to ship Phase 9 without precompiled-Rhai-module support and revisit when an alternative crate or upstream fix lands. Source-form Rhai modules loaded at startup time work without `rhai-dylib`; the audit only gates the precompiled-`.so` path.

### 13.13 Spike Contingency

The inventory-on-dylib spike (see [`docs/research/2026-05-10-inventory-on-dylib-spike.md`](../research/2026-05-10-inventory-on-dylib-spike.md), TBD) is validating whether `cargo rustc --crate-type=dylib` works for djogi AND whether `inventory::submit!` registrations made inside djogi remain visible to consumers that link the resulting dylib. Phase 9 is specced assuming the spike's best outcome; this section names the contingencies for the other three.

**Best case — `GO_NATIVE`.** `cargo rustc --crate-type=dylib` produces a working `libdjogi.so` and inventory registrations propagate natively across the dylib boundary. Phase 9 ships exactly as specced above: shell binary dlopens the dylib, all model/descriptor/runtime calls dispatch through it, no special build configuration beyond a per-target `cargo rustc` invocation. **No changes to djogi's `Cargo.toml` required.**

**Contingency 1 — `GO_WITH_MANIFEST`.** `cargo rustc --crate-type=dylib` works but inventory propagation only succeeds when djogi's `Cargo.toml` declares the dylib at manifest level rather than per-invocation. Resolution: djogi's `Cargo.toml` adds:

```toml
[lib]
crate-type = ["lib", "dylib"]
```

Both crate types are emitted on every build. Adopters who consume djogi as a normal `lib` dependency (the overwhelming majority) see no behavioral change; the shell and lihaaf consume the `dylib` artifact. Build time grows modestly (one extra link step per djogi build); binary size in `target/` grows by the dylib artifact. Acceptable cost for the iteration-loop and plugin benefits.

**Contingency 2 — `GO_WITH_WORKAROUND`.** Both `cargo rustc` and the manifest-level declaration produce a working dylib but inventory propagation fails — the dylib boundary breaks the static-section trick `inventory` uses to collect submissions at startup. Resolution: djogi exposes explicit per-collection `pub fn lihaaf_inventory_collect_<T>()` re-exports for every inventory-collected type (`ModelDescriptor`, `AppDescriptor`, etc.). The shell and lihaaf call these explicit collection functions at startup instead of relying on `inventory::iter`. Slightly verbose at the call site; functionally equivalent. The naming convention `lihaaf_inventory_collect_*` is shared with the lihaaf crate so both consumers reuse the same surface.

**Contingency 3 — `NO_GO`.** `cargo rustc --crate-type=dylib` fails outright on djogi (proc-macro dependencies, build-script outputs, or workspace shape rejects dylib emission). Resolution: Phase 9 ships with a statically-linked shell binary. The build-iteration, plugin-ecosystem, and distribution-size benefits are deferred until the underlying blocker resolves (likely a Rust toolchain or workspace-config fix). The shell still works — startup is slower, the binary is larger, `rhai-dylib` plugin loading is unsupported until the dylib path opens. Phase 9's parse-vs-eval split, djqry authoring loop, and ergonomics improvements all ship regardless; only the dylib-dependent items defer. **This is the only contingency that materially reshapes Phase 9's deliverable set; the spike is sized to surface it early enough to avoid building toward an architecture that won't compile.**

The spike's outcome is captured in the shell's Phase 9 task list before any dylib-dependent work begins. See `implementation-plan.md` §9 for the corresponding task graph and sequencing.

---
