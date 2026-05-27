# Apps

Apps are Djogi's compile-time schema-ownership domains. An app is a logical grouping of models that share a migration boundary — Phase 7's migration differ groups work by `(database_target, app_label)`, producing one `<database>/<app>/` directory on disk for each pair.

```rust
djogi::apps! {
    #[app(database = "main")]
    pub struct Vehicles;

    #[app(database = "main")]
    pub struct Users;

    #[app(database = "crud_log")]
    pub struct Audit;
}
```

Each entry expands to a zero-sized unit struct with a sealed `App` trait impl. Models reference apps by **type path**, not by string label:

```rust
#[model(table = "cars", app = Vehicles)]
pub struct Car {
    pub make: String,
}
```

Rust's own name resolution enforces declaration — `#[model(app = NotAnApp)]` fails with a standard rustc error.

Models without `#[model(app = ...)]` fall into the synthetic global bucket, which targets `main` by default. Apps are opt-in.

## App attributes

```rust
#[app(
    database = "main",        // required — database target
    label    = "billing",     // optional — override auto-derived label
    renamed_from = "billing_old",  // optional — former label for rename support
    tombstone,                // optional — app is being retired (mutually exclusive with renamed_from)
)]
```

- **`database`** — required. One of `"main"`, `"crud_log"`, `"event_log"`, or a user-defined target. No default. An app without an explicit `database` is a compile error so tables never silently land in the wrong place.
- **`label`** — optional. Defaults to the struct identifier lowercased byte-by-byte (`Vehicles` → `"vehicles"`). Override when the default would be awkward (`BillingAccounts` → default `"billingaccounts"`, probably want `label = "billing_accounts"`).
- **`renamed_from`** — optional. Declares that this app is the continuation of a prior label. Phase 7's differ generates a `ALTER SCHEMA ... RENAME` instead of drop-and-create.
- **`tombstone`** — optional flag. Marks the app for retirement; see the next section.

`(database, label)` is the identity pair. Two apps with the same label but different databases are legitimate — `main/audit/` and `crud_log/audit/` are distinct migration directories.

## Model attributes

```rust
#[model(
    table = "invoices",
    app   = Billing,               // optional — model belongs to this app
    moved_from_app = OldBilling,   // optional — historical metadata
)]
pub struct Invoice { /* ... */ }
```

- **`app`** — optional. Type path to a `djogi::apps!`-declared struct. `None` places the model in the synthetic global bucket.
- **`moved_from_app`** — optional. Type path to a prior app. Enables Phase 7's differ to track model-across-app moves without forcing the old app to stay declared.

---

## Retirement flow (tombstones)

Retiring an app takes **two compose cycles**. The tombstone marker sits in source for exactly that window — it's a transitional signal, not a permanent annotation.

### Why not just delete the app?

You could: delete `pub struct OldBilling;` from the apps block and let the migration differ infer retirement from snapshot diff. But that makes destructive migrations an apply-time decision in the migration runner rather than a PR-level one. Destructive changes should clear the *higher* bar. The tombstone marker:

- Surfaces the retirement in the PR diff (`+ #[app(tombstone)]`) — every code reviewer sees it immediately.
- Enforces the two-step move-then-retire flow via a compile-fail rule that stops you from tombstoning an app that still has active models pointing at it.
- Produces a specific error message pointing at the fix (`moved_from_app = OldApp`) instead of a generic "path not found" when active references are left dangling.

### The two-cycle flow

Suppose you want to retire `OldBilling` and move its one model, `Invoice`, into `Billing`.

**Cycle 1 — move models off `OldBilling`:**

```rust
djogi::apps! {
    #[app(database = "main")]
    pub struct OldBilling;      // still declared, still live

    #[app(database = "main")]
    pub struct Billing;
}

#[model(table = "invoices", app = Billing, moved_from_app = OldBilling)]
pub struct Invoice { /* ... */ }
```

Compose + apply this. Phase 7's differ sees `moved_from_app = OldBilling` on `Invoice`, generates `ALTER TABLE oldbilling.invoices SET SCHEMA billing` (or equivalent), and the table physically moves. `OldBilling` still exists in source but now owns no models.

**Cycle 2 — tombstone `OldBilling`:**

```rust
djogi::apps! {
    #[app(database = "main", tombstone)]
    pub struct OldBilling;      // tombstoned — retiring

    #[app(database = "main")]
    pub struct Billing;
}

#[model(table = "invoices", app = Billing, moved_from_app = OldBilling)]
pub struct Invoice { /* ... */ }
```

Compose + apply this with `--allow-destructive`. The differ generates the final retirement SQL (drops any leftover tables under `OldBilling` — there shouldn't be any after Cycle 1 — and marks the `main/oldbilling/` directory as tombstoned in the ledger). `moved_from_app = OldBilling` on `Invoice` is still legal — tombstoned apps are valid `moved_from_app` targets by design. That's the whole point of the attribute: historical metadata persists across retirement.

**Cycle 3 (or any later commit) — remove the source:**

Once Cycle 2's migration applies in prod, you can delete both:

```rust
djogi::apps! {
    #[app(database = "main")]
    pub struct Billing;
    // OldBilling struct gone; moved_from_app references below must go too
}

#[model(table = "invoices", app = Billing)]
pub struct Invoice { /* ... */ }
```

The `moved_from_app = OldBilling` annotation on `Invoice` also disappears (the path would no longer resolve otherwise). The ledger has already recorded the move; source no longer needs to carry the bridge.

### Rules the compiler enforces

- **`tombstone` and `renamed_from` are mutually exclusive** within one `#[app(...)]`. Retirement and rename are different operations.
- **`renamed_from = "live_label"` is a compile error** if a live app in the same `djogi::apps!` block still uses that label. A rename retires the old label — it can't coexist with the new one.
- **`#[model(app = TombstonedApp)]` on an active model is a compile error.** The error message suggests the fix: use `#[model(app = NewApp, moved_from_app = TombstonedApp)]` if you meant to move the model, not keep it on the retiring app.
- **`#[model(moved_from_app = TombstonedApp)]` is legal.** Historical metadata can (and usually does) point at tombstoned apps.

### Timing

The tombstone period is short — one or two compose cycles at most. If you find yourself carrying a tombstoned struct across many cycles, something is wrong (probably the Cycle 2 migration hasn't been applied in prod yet, or a `moved_from_app` reference is still blocking source deletion).

---

## Migration grouping

On disk, migrations nest by `(database_target, app_label)`:

```text
migrations/
├── main/
│   ├── vehicles/
│   │   ├── schema_snapshot.json
│   │   ├── V20260301000000__initial.sdjql
│   │   └── V20260301000000__initial.down.sdjql
│   └── billing/
│       └── ...
├── crud_log/
│   └── audit/
│       └── ...
└── event_log/
    └── ...
```

Each database target has its own ledger; Djogi does not pretend a single migration apply session across multiple targets is a distributed transaction. The differ applies one target at a time.

The synthetic global bucket (`""` label) files under `<default-database>/` without a nested app directory.

---

## Runtime lookup

`djogi::apps::AppRegistry::all()` returns every registered `AppDescriptor` plus the synthetic global bucket, sorted by `(label, database)`. Duplicate `(database, label)` identity pairs across invocations panic at first call — the migration differ runs before any SQL, so the panic lands at startup.

```rust
use djogi::apps::AppRegistry;

for desc in AppRegistry::all() {
    println!("{}/{}: tombstone={}", desc.database, desc.label, desc.tombstone);
}
```

The `App` trait exposes per-app const access for consumers that know an app at compile time:

```rust
const _: &str = <Vehicles as djogi::App>::LABEL;
const _: bool = <OldBilling as djogi::App>::TOMBSTONE;
```

Phase 7's migration differ prefers `AppRegistry::all()` since it needs to iterate everything.

---

## Sealing and enforcement

The `djogi::App` trait is **convention-sealed**. A determined downstream crate can technically reach into `#[doc(hidden)] pub` items and hand-write an `impl djogi::App for MyFake`, but this is unmistakably an act of "I am reaching into internal API." True hard-sealing of a proc-macro-emitted trait is not achievable in stable Rust when the proc macro lives in a separate crate — every pub path the macro reaches is also reachable by handwritten downstream code.

The correctness invariant that matters — "a forged `App` impl cannot silently break migrations" — is enforced at the use site by Phase 7's migration differ: every `#[model(app = X)]` is cross-checked against `AppRegistry::all()` before the migration library applies SQL, and any model pointing at an `App`-implementing type whose `AppDescriptor` is missing from inventory hard-errors before any SQL executes. Forged `App` impls compile, but they're inert.
