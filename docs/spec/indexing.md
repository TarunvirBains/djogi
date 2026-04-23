> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# Indexing

## 20. Indexing

Djogi treats indexes as first-class schema objects. A model declares them once — at the struct — and the same declaration drives the descriptor, the migration differ, the generated DDL, and the canonical index name. There is no separate "index migration" step; adding an `index(...)` entry to a `#[model(indexes(...))]` list is a real schema change that the Phase 7 differ picks up on the next `djogi migrations compose`.

This chapter is the authoritative surface for the index contract. The runtime types live in [`djogi::descriptor`]; the parser lives in `djogi-macros::model::indexes`; the Phase 7 differ consumes `IndexSpec` directly.

---

### 20.1 Where indexes come from

A model carries indexes from three sources:

1. **Implicit** — the primary key, `#[field(unique)]`, and `#[field(index)]` annotations. These are emitted by the descriptor automatically and never appear in `indexes(...)`.
2. **Spatial** — any `GeoPoint` or other `GeographyValue` field emits a GiST index on the underlying `geography` column. Gated by the `spatial` feature flag; emitted with `requires_out_of_transaction = true` because GiST builds on `GEOGRAPHY` in PostGIS 3.x can deadlock under concurrent writes.
3. **User-declared** — `#[model(indexes(...))]`. The surface this chapter covers.

All three sources merge into the final `&[IndexSpec]` slice the descriptor emits. The merge order is alphabetised-by-name so minor source reorderings do not produce spurious migration diffs.

---

### 20.2 The grammar (§5 of the v3 plan)

Each entry inside `indexes(...)` is either `index(...)` (non-unique by default) or `unique(...)` (unique by default). The body keys are identical between the two — only the default kind differs.

| Key | Shape | Meaning |
|-----|-------|---------|
| `fields` | `= [ident, ...]` or `= [(col = ident, opclass = "...", order = asc\|desc, nulls = first\|last\|default), ...]` | Column list. Mutually exclusive with `expr`. |
| `expr` | `= "lower(email)"` | Expression target. Mutually exclusive with `fields`. |
| `using` | `= "btree" \| "gin" \| "gist" \| "brin" \| "hash"` | Access method. Default: `btree`. |
| `opclass` | `= "text_pattern_ops"` | Single-column opclass declaration shortcut. |
| `include` | `= [ident, ...]` | Covering-index payload columns (`INCLUDE(...)`). |
| `where` | `= "deleted_at IS NULL"` | Partial-index predicate. Raw SQL — see §20.4. |
| `nulls_not_distinct` | `= true` | Unique-only — treat `NULL`s as equal. Forces the `UniqueIndex` kind. |
| `concurrently` | `= true` | Emit `CREATE INDEX CONCURRENTLY`. See §20.6 for the full contract. |
| `name` | `= "custom_idx"` | Override the deterministic generated name. |

**Rules baked into the macro (compile-fail when violated):**

- An entry must supply exactly one of `fields` or `expr`. Missing both is an error; supplying both is an error.
- `hash` indexes are unique-incompatible (hash indexes cannot enforce uniqueness) and multi-column-incompatible (Postgres hash indexes are single-column). `where`, `include`, and expression targets are also rejected — hash indexes support none of them.
- `nulls_not_distinct = true` on a non-`unique(...)` entry is rejected. (It has no meaning on a plain index.)
- A raw-identifier column reference (`r#yield`) normalises to its unraw form before the column-existence check — `fields = [r#yield]` matches `pub r#yield: String`.
- A `name = "..."` override must not collide with a name the emitter would generate for any other declared index (implicit, spatial, or user-declared). Collisions fail at macro expansion with a span-precise error.

---

### 20.3 Column ordering is semantic

`index(fields = [last, first])` and `index(fields = [first, last])` are **different indexes with different names**. Djogi preserves declared column order byte-for-byte in the descriptor, the emitted SQL, the canonical index name, and the migration diff. The differ never reorders columns on its own.

This matches Postgres: a composite BTree index on `(a, b)` accelerates `WHERE a = ? AND b = ?`, `WHERE a = ?`, and `ORDER BY a, b`, but does nothing for `WHERE b = ?` or `ORDER BY b, a`. Swapping the column order changes which queries benefit.

Two consequences:

- Renaming columns does not reorder the index. A field rename annotated with `#[field(renamed_from = "...")]` propagates the new name into the index spec but preserves its position.
- Reordering `fields = [...]` in source **is** a schema change. The old index gets dropped and the new one created under a different name.

---

### 20.4 Predicate validation policy

Predicates are passed through as raw SQL strings. Djogi does not parse, lint, or shape-validate them — the string lands verbatim in the emitted DDL and Postgres validates at migration-apply time. An invalid predicate surfaces as a migration error with the full server-side diagnostic (column not found, type mismatch, undefined function, etc.).

Rationale:

- Writing a reliable SQL parser inside the macro is a very large engineering project; the benefit is at most "the error surfaces seconds earlier" because `djogi migrations compose` runs a dry-apply check.
- A DSL — "only these operators, only these functions" — would lag Postgres and permanently limit what users can express.
- The no-regex rule (`docs/spec/decisions.md`) rules out pattern-matching shortcuts.

Pass whatever the Postgres predicate grammar accepts. If the migration fails, read the diagnostic; the raw text is what you wrote.

---

### 20.5 Unique constraint vs unique index

`unique(...)` lowers to one of two Postgres objects depending on what the declaration requires:

| Declaration includes | Lowers to | Name suffix |
|----------------------|-----------|-------------|
| none of the below | `ALTER TABLE ... ADD CONSTRAINT ... UNIQUE (...)` | `..._key` |
| `where = "..."` | `CREATE UNIQUE INDEX ...` | `..._uidx` |
| `include = [...]` | `CREATE UNIQUE INDEX ...` | `..._uidx` |
| `nulls_not_distinct = true` | `CREATE UNIQUE INDEX ...` | `..._uidx` |
| `expr = "..."` | `CREATE UNIQUE INDEX ...` | `..._uidx` |
| `concurrently = true` | `CREATE UNIQUE INDEX ...` | `..._uidx` |

Unique constraints are the default for ordinary uniqueness because they integrate with `REFERENCES`, `ON CONFLICT`, and the constraint catalogue. Unique indexes exist for the cases where Postgres requires one — partial uniqueness, `INCLUDE`, `NULLS NOT DISTINCT`, expression targets, or concurrent builds (a constraint cannot be added `CONCURRENTLY`; it has to be a pre-existing `CREATE UNIQUE INDEX CONCURRENTLY` that a follow-up `ALTER TABLE ... ADD CONSTRAINT ... UNIQUE USING INDEX` adopts).

The macro picks automatically. Users do not have to know the distinction for the common cases.

---

### 20.6 The `concurrently = true` contract

Djogi's concurrency model for index builds is **deterministic** (Phase 7-Zero v3 Q1 ruling). `concurrently = true` at declaration means:

- the emitted DDL is `CREATE INDEX CONCURRENTLY` (or `CREATE UNIQUE INDEX CONCURRENTLY`) in **every** profile — prod, CI, dev, and test;
- the migration file containing that index is marked non-transactional in every profile;
- there is no auto-detection: omitting the attribute on a hot table does not trigger a rewrite. The operator is responsible for declaring intent correctly.

Because this model accepts a user-facing foot-gun in exchange for CI/prod parity, every documentation surface that mentions `concurrently = true` must cover all eight items below:

#### 1. What `CREATE INDEX CONCURRENTLY` does

Builds the index under a weak `SHARE UPDATE EXCLUSIVE` table lock — reads and writes continue against the table throughout the build. Postgres runs a two-pass scan (the second pass catches rows the first pass raced against), so the wall-clock build is slower than a non-concurrent build — but no writer is ever blocked.

(Plain `CREATE INDEX` takes `SHARE`, which permits concurrent reads but blocks every writer for the duration of the scan. Neither flavour takes `ACCESS EXCLUSIVE`, but the practical effect of `SHARE` on a hot table is a write-only outage — see item 4.)

#### 2. When to use it

Adding an index to a production table that already contains meaningful data — especially a write-hot table where blocking writes would be visible to users. Default bias: **if in doubt on a table that will ship to production, set it.**

#### 3. When not to use it

- Small tables where the non-concurrent build completes in milliseconds.
- Tables created fresh in the same migration (they have no users yet and no rows to race against).
- CI-only test tables with trivial data.

Concurrent builds add overhead — more disk I/O, longer wall-clock, a transaction-per-pass — without benefit in these cases.

#### 4. The foot-gun

Omitting `concurrently = true` on an index added to a large production table takes a `SHARE` lock and **blocks every write to that table for the duration of the build**. Reads continue, but every INSERT / UPDATE / DELETE — and every transaction holding one — queues until the index finishes building. On a multi-gigabyte write-hot table, that can be minutes of effective write-side downtime.

**Djogi does not detect this for you.** The operator owns the per-index decision. The §6.5 apply-time advisory warning (see item 7) is a rescue, not a guarantee.

#### 5. Failure mode

A `CONCURRENTLY` build that fails partway through leaves an `INVALID` index in `pg_index`. Postgres will not use an invalid index for queries, but it still occupies disk and accumulates during repeated retries. The operator must drop invalid indexes manually before retrying:

```sql
SELECT n.nspname, c.relname
FROM pg_index i
JOIN pg_class c ON c.oid = i.indexrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE i.indisvalid = false;

DROP INDEX <schema>.<index_name>;
```

#### 6. Determinism promise

The same DDL runs in CI and in production. A migration containing any `concurrently = true` index is marked non-transactional in every profile — CI migrations do not quietly become transactional to simplify rollback. Tests see the same failure modes production sees. This is Phase 7-Zero v3 Q1 ruling A; the alternative (downgrade in CI to `CREATE INDEX` wrapped in a transaction) was rejected because it hides production-only failure modes until they happen in production.

#### 7. The apply-time advisory warning (Phase 7-Zero §6.5)

When a migration adds an index *without* `concurrently = true` to a table whose current size exceeds the configured page threshold, the runner prints a WARN (ERROR in strict mode). The message names the table, the index, the threshold, and the one-line fix — add `concurrently = true` to the declaration and regenerate the migration.

Configure in `Djogi.toml`:

```toml
[migrations.advisory]
concurrent_index_warning_page_threshold = 10000   # pages (8 KiB each); default tunable
strict_mode = false                               # true = ERROR instead of WARN
```

The warning is a rescue only. It fires on the table's current size, not its projected production size; a table that is small in CI and large in production will pass CI silently. Declare intent correctly at the source.

#### 8. Descending-sort PK types (§4.1)

`pk = "heerid_desc"` and `pk = "ranjid_desc"` are alternatives to a reverse secondary index for workloads dominated by descending-chronological scans ("latest N", keyset pagination newest-first, "recent activity" lists).

**What the XOR flip does, conceptually.** A HeerId or RanjId encodes a timestamp in its high bits. A plain BTree index on the column scans oldest-first. The `_desc` variants XOR the identifier bits before storage so the same BTree index scans newest-first — no secondary index, no reverse-order plan step, no extra write amplification.

**One-question decision rule:**

> Does ascending-chronological access on this table also matter?
>
> **Yes** (forward timelines, oldest-to-newest audits, chronological exports) → use the ascending PK plus a reverse secondary index.
>
> **No** (newest-first feeds, latest-N dashboards, keyset pagination of "recent X") → use `pk = "heerid_desc"` / `"ranjid_desc"`.

**When to pick which:**

| Workload | Choice |
|----------|--------|
| Activity feeds, "recent orders", dashboards | `heerid_desc` / `ranjid_desc` |
| Audit timeline, seeded integration tests, time-range exports | default `heerid` / `ranjid` + reverse secondary index on the occasional descending query |
| Mixed read patterns (forward and reverse equally hot) | default PK + secondary index — avoid the migration cost below |

**Migration has a point of no return.** Flipping a table from ascending to descending (or back) is supported by Phase 7's `SchemaDelta::Classification::PkTypeFlip` path, but it rewrites every row, cascades to every FK that references the PK, and must run as a coordinated cutover — every child table with a `ForeignKey<Parent>` to the migrating PK migrates in the same Phase 7 migration, not a later follow-up. There is no per-row backfill window. Plan the flip for a maintenance slot and own the choice up front; the default is more often the right answer than first-time adopters expect.

---

### 20.7 Deterministic index naming

Index names are derived from the table name, the target, and the kind. Format:

```
<table>_<stem-body>_<suffix>
```

- `<stem-body>` is either the underscore-joined column names (`email`, `tenant_id_external_id`) or the literal `expr` for expression-target indexes.
- `<suffix>` is `idx` (non-unique), `key` (unique constraint), or `uidx` (unique index).

Example: `users_email_key`, `orders_tenant_id_created_at_idx`, `messages_expr_uidx`.

When the naïve name would exceed Postgres' 63-byte identifier limit, the stem is truncated to 55 bytes and an 8-character hex digest of the full pre-truncation name is appended so near-duplicate inputs cannot collide. The hash uses `std::hash::DefaultHasher` (SipHash-1-3 with a fixed seed), which is deterministic across runs of the same Rust toolchain — identical descriptor inputs produce byte-identical names on every build. The migration emitter and the runtime `index_name` helper both use the same hasher and are pinned to byte-for-byte parity by a unit test.

**Toolchain-upgrade caveat:** Rust's standard library reserves the right to change `DefaultHasher`'s algorithm across releases. If that happens, index names that previously needed truncation would change on the first build under the new toolchain, surfacing as additive migration diffs (new index with a different name, old index dropped). Names that do not need truncation — the vast majority — are unaffected. A future Djogi release may pin a fixed hasher if this becomes a real-world pain point; for now, names stay stable within a toolchain and renames under a toolchain upgrade are a recoverable migration event, not data loss.

Both the descriptor runtime (`djogi::descriptor::index_name`) and the macro emitter produce byte-for-byte identical names — a parity test in the macro crate pins the equivalence. Users who want a different name supply `name = "..."` at declaration; the override is validated against the same collision rules as the generated name.

---

### 20.8 Raw identifiers and reserved words

Columns whose names collide with Rust keywords (`r#yield`, `r#async`, `r#move`, …) are declared on the struct with the `r#` prefix and referenced in `fields = [...]` with the same prefix. The macro normalises via `syn::ext::IdentExt::unraw` before matching against declared columns, so `fields = [r#yield]` matches `pub r#yield: String`.

This is a Rust-side concern only. The column name stored in the descriptor and emitted in DDL is `yield` (Postgres reserves a different set of keywords than Rust; the two lists overlap but are not identical). If a column name collides with a Postgres reserved word, quote it at the table level by picking a different column name — Djogi does not emit `"quoted"` identifiers in generated DDL.

---

### 20.9 Descriptor contract

The runtime types the migration differ consumes:

```rust
pub struct IndexSpec {
    pub name: &'static str,
    pub target: IndexTarget,
    pub kind: IndexKind,
    pub index_type: IndexType,
    pub predicate: Option<&'static str>,
    pub include: &'static [&'static str],
    pub nulls_not_distinct: bool,
    pub requires_out_of_transaction: bool,
    pub extension_dependency: Option<&'static str>,
}

pub enum IndexKind { NonUnique, UniqueConstraint, UniqueIndex }
pub enum IndexTarget {
    Columns(&'static [IndexColumnSpec]),
    Expression(&'static str),
}

pub struct IndexColumnSpec {
    pub name: &'static str,
    pub opclass: Option<&'static str>,
    pub order: IndexOrder,
    pub nulls: IndexNullsOrder,
}
```

The full rustdoc lives on each item. Phase 7-Zero v3 §4 is the frozen contract — the Phase 7 differ is entitled to assume these shapes; field additions are additive-only (new `Option` / `&'static [T]` fields with an `IndexSpec::simple` constructor handling defaults).

`IndexColumnSpec::simple("name")` is the one-column convenience constructor: no opclass, `Asc`, default nulls. Multi-column simple declarations remain one-liners:

```rust
IndexTarget::Columns(&[
    IndexColumnSpec::simple("tenant_id"),
    IndexColumnSpec::simple("created_at"),
])
```

---

### 20.10 Cross-references

- [Models](./models.md) §4.5 — field-level `#[field(index)]` / `#[field(unique)]`.
- [Migrations](./migrations.md) — the Phase 7 differ's consumption of `IndexSpec`, the apply-time advisory warning, and the non-transactional migration rule for `concurrently = true`.
- [Primary Keys](./primary-keys.md) — HeerId / RanjId / HeerIdDesc / RanjIdDesc semantics.
- [Decisions](./decisions.md) — rows for concurrent index creation, unique-constraint default, column ordering, per-column spec, predicate validation.
- `docs/superpowers/plans/2026-04-22-phase7-zero-indexing-v3.md` §§4–6 — the frozen contract, grammar, lowering rules, and apply-time advisory-warning specification.
