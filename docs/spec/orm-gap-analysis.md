> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# ORM Gap Analysis: Django 6.0 vs Djogi

*Based on a deep dive into the Django 6.0 source code (`stable/6.0.x`). Reference clone at `../django-reference/`.*

This document maps every functional capability in Django's ORM, identifies what Djogi's current spec covers, what's missing, and — critically — where Djogi can do **better** than Django by leveraging Rust's type system and Postgres-only targeting.

---

## 1. QuerySet Methods

### Currently in Djogi spec

| Capability | Djogi Status |
|---|---|
| `filter()` | Covered (closure + programmatic) |
| `order_by()` | Covered |
| `limit()` | Covered |
| `fetch_all()` / `fetch_one()` / `first()` / `count()` | Covered |
| `get()` by PK | Covered |
| `create()` | Covered |
| `save()` | Covered |
| `delete()` (instance) | Covered |
| `prefetch()` (FK relations) | Covered |
| `raw_filter()` | Mentioned as escape hatch |

### Missing from Djogi spec — Must Add

| Django Method | What It Does | Djogi Recommendation |
|---|---|---|
| **`exclude()`** | Negative filter (`NOT WHERE`) | Add as `.exclude(\|f\| ...)` — same closure API as filter |
| **`annotate()`** | Add computed columns to results | Add — critical for real-world queries. See §2 Expressions. |
| **`aggregate()`** | Terminal: return `{name: value}` dict for Sum/Avg/etc | Add — returns a struct, not a QuerySet |
| **`values()` / `values_list()`** | Select specific columns, return tuples/maps instead of full models | Add as `.select(\|f\| (f.make, f.gas_fill))` returning typed tuples |
| **`distinct()`** | `SELECT DISTINCT` (Postgres supports field-level DISTINCT ON) | Add — Postgres DISTINCT ON is a strength |
| **`exists()`** | Optimized `SELECT 1 ... LIMIT 1` boolean check | Add |
| **`update()`** (on QuerySet) | Bulk `UPDATE ... SET` without loading instances | Add — `.filter(...).update(\|f\| f.gas_fill.set(100))` |
| **`delete()`** (on QuerySet) | Bulk `DELETE` without loading instances | Add — `.filter(...).delete()` |
| **`select_related()`** | JOIN-based eager loading (single query, FK/O2O only) | Add — Djogi only has `prefetch()` (separate queries). JOIN-based is faster for FK chains. |
| **`select_for_update()`** | `FOR UPDATE` row locking | Add — essential for concurrent writes. Support `nowait`, `skip_locked`, `of()`, `no_key` |
| **`bulk_create()`** | Batch INSERT | Partially covered. Formalize with `ignore_conflicts` and `update_conflicts` (upsert) |
| **`bulk_update()`** | Batch UPDATE via CASE/WHEN | Add |
| **`get_or_create()`** | Atomic lookup-or-insert | Add — returns `(instance, created: bool)` |
| **`update_or_create()`** | Atomic lookup-update-or-insert | Add — upsert pattern |
| **`in_bulk()`** | Fetch multiple PKs → HashMap | Add — `Vehicle::in_bulk(&mut ctx, &[id1, id2, id3])` |
| **`only()` / `defer()`** | Partial field loading | Add — select only needed columns. Rust can enforce at type level (phantom types or builder return types) |
| **`none()`** | Empty QuerySet that never queries | Add — useful for conditional composition |
| **`reverse()`** | Reverse current ordering | Add |
| **`union()` / `intersection()` / `difference()`** | Set operations (UNION / INTERSECT / EXCEPT) | Add |
| **`explain()`** | Return EXPLAIN output | Add — invaluable for debugging |
| **`iterator()` / chunked evaluation** | Memory-efficient streaming without caching | Add — use Postgres cursors via `tokio-postgres` `query_raw()` stream |
| **`earliest()` / `latest()`** | Get by ordering field | Add — convenience over `.order_by(...).first()` |
| **`contains(obj)`** | Check if instance is in QuerySet | Add |
| **`using()`** | Select database | Answered by `QuerySet::with_read_mode(ReadMode::...)` in Phase 12 — see [Distributed Topology & Residency](./topology.md). Djogi declares the hint; the pool-selection strategy configured by the application honors it. |

### Where Djogi Can Do Better

| Django Weakness | Djogi Opportunity |
|---|---|
| `values()` returns untyped dicts | Djogi can return **typed tuples** via `.select(\|f\| (f.make, f.gas_fill))` — compile-time column selection |
| `only()`/`defer()` defer to runtime; accessing deferred field triggers implicit query | Djogi can use **phantom types** to make partial models a distinct type — accessing unloaded fields is a compile error, not a runtime surprise |
| `select_related()` follows all FKs by default if no args (N+1 risk in reverse) | Djogi: always explicit. No default eager loading. |
| `bulk_create` doesn't call `save()` or signals | Djogi: document clearly. Consider optional signal dispatch via feature flag. |
| `update()` silently ignores non-existent fields | Djogi: compile-time field validation via typed closures |
| QuerySet caching (evaluated QS caches all results in memory) | Djogi: no implicit caching. `.fetch_all()` returns `Vec<T>`. Streaming via `.stream()` for large results. |

---

## 2. Expression System

### What Django Has

Django's expression system is massive (~134 classes). The core abstraction: expressions form a tree, each node compiles to `(sql_string, params)`. Key types:

#### Must Add to Djogi

| Expression | Django | Djogi Equivalent | Priority |
|---|---|---|---|
| **F() — field references** | `F('price')` → references another column | `Expr::field(\|f\| f.price)` or similar | **Critical** — needed for `update(price=F('price') + 10)` |
| **Value() — literal wrapper** | `Value(42)` | Auto-wrap literals in expressions | High |
| **Q() — boolean combinators** | `Q(a=1) \| Q(b=2)` for OR/NOT | Already covered by closure API `.or()` / `.and()`, but need standalone Q-like objects for dynamic construction | High |
| **Subquery / Exists / OuterRef** | Correlated subqueries | `Subquery(QuerySet)`, `Exists(QuerySet)`, `OuterRef(\|f\| f.id)` | High |
| **Case / When** | Conditional expressions | `Expr::case().when(cond, then).default(else)` | High |
| **Arithmetic on fields** | `F('a') + F('b')`, `F('price') * 1.1` | Operator overloading on field expressions | High |
| **Aggregates in annotations** | `.annotate(avg_price=Avg('price'))` | See §3 | Critical |
| **Window functions** | `Window(Rank(), partition_by=..., order_by=...)` | Full Postgres window function support | Medium |
| **OrderBy with NULLS FIRST/LAST** | `OrderBy(F('name'), nulls_last=True)` | `.order_by(\|f\| f.name.asc().nulls_last())` | High |
| **Database functions** | Cast, Coalesce, Greatest, Least, NullIf, Lower, Upper, Concat, etc. | Provide as `djogi::functions::*` — only Postgres variants needed | Medium |

#### Where Djogi Can Do Better

| Django Weakness | Djogi Opportunity |
|---|---|
| F() is stringly-typed: `F('nonexistent')` fails at runtime | Djogi: **typed field references** via closures. `\|f\| f.price` is a compile error if `price` doesn't exist. |
| Expression output type inference is runtime guesswork | Djogi: expressions carry Rust types. `F(price) + F(quantity)` knows it returns `i32` at compile time. |
| Backend dispatch via `as_postgresql()` / `as_mysql()` etc. | Djogi: Postgres-only = **one codepath**. No multi-backend dispatch overhead. Every function compiles directly to Postgres SQL. |
| 134+ classes in the expression tree | Djogi: can be leaner. Many Django functions exist to paper over backend differences. Postgres-only means ~40% of them are unnecessary. |

---

## 3. Aggregation & Annotation

### Must Add

| Aggregate | SQL | Notes |
|---|---|---|
| `Count` | `COUNT(*)` / `COUNT(DISTINCT col)` | Support distinct, filter |
| `Sum` | `SUM(col)` | |
| `Avg` | `AVG(col)` | |
| `Max` | `MAX(col)` | |
| `Min` | `MIN(col)` | |
| `StringAgg` | `STRING_AGG(col, delimiter)` | Postgres-native (Django had to shim this for other DBs) |
| `ArrayAgg` | `ARRAY_AGG(col)` | Postgres-only — Django has this in `contrib.postgres`. Djogi should have it natively. |
| `JsonAgg` | `JSON_AGG(col)` / `JSONB_AGG(col)` | Postgres-only — not in Django core. Djogi advantage. |

**Aggregate FILTER clause:** Postgres supports `COUNT(*) FILTER (WHERE active = true)`. Django wraps this. Djogi should expose it natively since it's Postgres-only.

### Annotation API

```rust
// Annotate each owner with their vehicle count
Owner::objects()
    .annotate("vehicle_count", Count(|f| f.vehicles))  // reverse relation
    .filter(|f| f.vehicle_count.gte(5))
    .fetch_all(&mut ctx).await?;
```

This is the single biggest functional gap in the current Djogi spec. Real applications need aggregation constantly.

### Where Djogi Can Do Better

| Django | Djogi |
|---|---|
| `annotate()` returns same model type with extra attrs accessed by string name | Djogi can return **extended types** — annotated fields are part of the return struct, checked at compile time |
| Django's aggregation with multiple JOINs produces incorrect results (documented bug) — must use Subquery | Djogi: document this upfront and provide first-class Subquery support |
| Postgres-specific aggregates (ArrayAgg, JsonAgg, StringAgg) are in `contrib.postgres` | Djogi: first-class citizens since we're Postgres-only |

---

## 4. Lookup Types

### Currently in Djogi spec

`eq`, `neq`, `gte`, `gt`, `lte`, `lt`, `in_list`, `is_null`, `contains` (ILIKE), `starts_with` (ILIKE), `between`

### Missing — Should Add

| Lookup | SQL | Notes |
|---|---|---|
| `exact` (case-sensitive) | `= $1` | Djogi's `eq` covers this |
| `iexact` | `ILIKE $1` (exact, case-insensitive) | Add |
| `icontains` | Already covered as `contains` | Clarify: Djogi's `contains` is ILIKE. Add case-sensitive variant too. |
| `endswith` / `iendswith` | `LIKE '%val'` / `ILIKE '%val'` | Add |
| `regex` / `iregex` | `~ 'pattern'` / `~* 'pattern'` | Add — Postgres regex is powerful |
| `range` | `BETWEEN $1 AND $2` | Already covered as `between` |
| **JSON lookups** | `@>`, `?`, `?&`, `?\|` | `has_key`, `has_keys`, `has_any_keys`, `contains` (JSON containment) — critical for Jsonb<T> |

### Where Djogi Can Do Better

| Django | Djogi |
|---|---|
| Lookups are stringly-typed: `filter(name__icontains="abc")` | Djogi: `.filter(\|f\| f.name.icontains("abc"))` — method on typed field reference |
| Django registers lookups dynamically; custom lookups require class ceremony | Djogi: custom lookups can be Rust traits. Or just drop to raw SQL. |

---

## 5. Model Instance Methods

### Currently in Djogi spec

`create()`, `save()`, `delete()`, `get()` by PK

### Missing — Should Add

| Method | What It Does | Priority |
|---|---|---|
| **`refresh_from_db()`** | Reload fields from DB | High — needed after bulk updates or concurrent modifications |
| **`full_clean()` / `clean()`** | Validation pipeline | Medium — Djogi has Jsonb validation but not model-wide validation hooks |
| **`validate_unique()`** | Check unique constraints before save | Medium |
| **`save(update_fields=[...])`** | Partial update — only save specified fields | High — dirty tracking covers this but explicit is useful too |

### Where Djogi Can Do Better

| Django | Djogi |
|---|---|
| `save()` does SELECT then UPDATE (or just INSERT). Detects insert vs update via PK presence. | Djogi: explicit. `create()` is INSERT, `save()` is UPDATE. No ambiguity. Already better. |
| Django's `save()` saves ALL fields by default (full row UPDATE) | Djogi: dirty tracking makes partial UPDATE the default when enabled. Already better. |
| `refresh_from_db()` reloads all fields or a subset | Djogi: can type-narrow — `car.refresh(&mut ctx, \|f\| (f.gas_fill, f.active))` returns only those |
| Django signals (pre_save, post_save, etc.) are runtime-registered, untyped | Djogi: consider compile-time hooks via trait impls instead of dynamic signals |

---

## 6. Field Types

### Currently in Djogi spec

`String`, `i32`, `i64`, `f64`, `bool`, `DateTime`, `HeerId`, `ForeignKey<T>`, `Jsonb<T>`, `Option<T>`

### Missing — Consider Adding

| Django Field | Rust Equivalent | Add? |
|---|---|---|
| `DecimalField` | `rust_decimal::Decimal` | Yes — financial data needs exact decimals |
| `UUIDField` | `uuid::Uuid` (for non-PK use) | Yes — already have RanjId for PKs, but plain UUID fields are common |
| `BinaryField` | `Vec<u8>` / `&[u8]` | Maybe — Postgres BYTEA |
| `DurationField` | `time::Duration` | Maybe — Postgres INTERVAL |
| `TextField` vs `CharField` | Often both carry `String`, but they are different schema primitives | Yes — Djogi should preserve `TEXT` vs `VARCHAR(n)` intent in descriptors and migrations |
| `SlugField`, `EmailField`, `URLField` | Validation wrappers over String | No — use validators, not distinct types. Unnecessary in a typed language. |
| `FileField` / `ImageField` | Out of scope | No — Djogi is an ORM, not a file storage framework |
| `GeneratedField` | `#[field(generated = "expression")]` | Yes — Postgres GENERATED ALWAYS AS is powerful |
| `CompositePrimaryKey` | Tuple PK | Defer — Django 6.0 just added this and FK to composite PK is unsupported even there |

### Where Djogi Can Do Better

| Django | Djogi |
|---|---|
| CharField requires `max_length` (historical MySQL limitation) | Djogi should not copy Django's baggage, but it should still preserve the bounded-vs-unbounded distinction because `VARCHAR(n)` and `TEXT` are different schema shapes in migrations. The Rust value may still be string-like; the descriptor must not collapse them into one field kind. |
| Django has ~30 field types, many are just CharField subclasses with validators | Djogi: fewer field types, more validators. Rust's type system handles the rest. |
| Django's `auto_now` / `auto_now_add` are field options that bypass `save()` | Djogi: `created_at` / `updated_at` are framework-injected with DB defaults. Cleaner. Already better. |

---

## 7. Delete Cascade System

### Currently in Djogi spec

`on_delete = "cascade"` or `"restrict"` (default RESTRICT)

### Missing — Should Add

| Handler | Behavior | Add? |
|---|---|---|
| `CASCADE` | Delete related objects | Already supported |
| `RESTRICT` | Prevent deletion (deferred check) | Already default |
| `PROTECT` | Prevent deletion (immediate check) | Add — alias for Postgres FK RESTRICT, but useful semantically |
| `SET_NULL` | Set FK to NULL | Add — requires `Option<ForeignKey<T>>` |
| `SET_DEFAULT` | Set FK to default value | Add |
| `SET(value)` | Set FK to specific value | Add |
| `DO_NOTHING` | No action (DB enforces) | Add |

### Where Djogi Can Do Better

| Django | Djogi |
|---|---|
| Django's Collector does an in-memory graph walk to resolve cascades, loading all related objects | Djogi: for simple cascades, let Postgres handle it (ON DELETE CASCADE is DB-level). Only do app-level collection when signals/CRUD logging need it. |
| `RESTRICT` vs `PROTECT` distinction is confusing in Django | Djogi: simplify. Default is Postgres RESTRICT. Document clearly. |

---

## 8. Model Inheritance

### Django supports three modes:
1. **Abstract** — no table, fields copied to children
2. **Multi-table** — each model gets a table, joined via OneToOneField
3. **Proxy** — same table, different Python class

### Djogi Recommendation

| Mode | Add? | Notes |
|---|---|---|
| Abstract | **Yes** — via Rust traits or shared field groups | Natural fit: define common fields in a trait/macro, compose into models |
| Multi-table | **No** — too much implicit magic (auto-JOIN, parent_ptr) | Against Djogi's explicit philosophy. If you need related tables, use explicit FK. |
| Proxy | **Maybe** — useful for different default ordering/managers on same table | Could be a `#[model(proxy_for = "Vehicle")]` that shares the table |

---

## 9. Admin Renderer — Resolved at Maahi Phase 10

This section previously argued for replacing Dioxus with HTMX + Askama for the admin renderer. The decision was reversed during Phase 10 design: Maahi (Djogi's admin console) ships as a Dioxus full-stack application. Pure-Rust component tree, type-safe server functions, desktop-renderer reach (`dioxus-desktop`), and richer interactivity ergonomics outweighed the bundle-size advantages of HTMX + Askama for djogi's adopter profile. To keep Dioxus's dep weight off non-admin adopters' lock files, Maahi is carved into its own `djogi-maahi` workspace crate behind the `admin` feature flag — see `CLAUDE.md` for the carve-out reasoning.

See [`docs/spec/maahi/`](./maahi/index.md) for the authoritative Maahi spec. A `djogi-light-admin` (HTMX + Askama only, no WASM toolchain) is parked in [`docs/roadmap/future-work.md`](../roadmap/future-work.md) if real demand surfaces.

---

## 10. Signals vs Hooks

### Django's Signal System

8 model signals: `pre_init`, `post_init`, `pre_save`, `post_save`, `pre_delete`, `post_delete`, `m2m_changed`, `class_prepared`

### Problems with Django Signals

1. Runtime-registered, no compile-time checking
2. Sender/receiver type safety is absent
3. Signal handlers can silently fail without breaking the caller
4. Hard to trace which signals are active in a codebase
5. Create hidden coupling — "action at a distance"

### Djogi: Trait-Based Hooks (Better)

```rust
impl ModelHooks for Vehicle {
    fn before_create(&mut self, pool: &PgPool) -> Result<()> {
        self.make = self.make.trim().to_uppercase();
        Ok(())
    }

    fn after_save(&self, pool: &PgPool, created: bool) -> Result<()> {
        if created {
            log::info!("New vehicle: {}", self.id);
        }
        Ok(())
    }

    fn before_delete(&self, pool: &PgPool) -> Result<()> {
        if self.active {
            return Err(DjogiError::validation("Cannot delete active vehicle"));
        }
        Ok(())
    }
}
```

Benefits:
- Compile-time: if the trait method signature changes, all implementations break at compile time
- Discoverable: `impl ModelHooks for X` is grep-able and IDE-navigable
- Typed: the hook receives the actual model instance, not a dynamic `sender` + `kwargs`
- Explicit: no "action at a distance" — the hook is defined on the model
- Optional: models without `impl ModelHooks` skip the hook dispatch entirely (zero cost)

---

## 11. Summary: Priority Tiers

### Tier 1 — Must Have for ORM Functionality Parity

- [ ] `exclude()` — negative filtering
- [ ] `annotate()` + aggregates (Count, Sum, Avg, Max, Min)
- [ ] `aggregate()` — terminal aggregate
- [ ] Field expressions (F-equivalent) — typed field references in updates/annotations
- [ ] `update()` on QuerySet — bulk update without loading
- [ ] `delete()` on QuerySet — bulk delete without loading
- [ ] `select_related()` — JOIN-based eager loading
- [ ] `exists()` — optimized existence check
- [ ] `distinct()` — with Postgres DISTINCT ON
- [ ] `get_or_create()` / `update_or_create()` — atomic upsert
- [ ] `select_for_update()` — row locking
- [ ] `values()` / `select()` — partial column selection with typed returns
- [ ] `Subquery` / `Exists` / `OuterRef` — correlated subqueries
- [ ] `Case` / `When` — conditional expressions
- [ ] Additional lookups: `iexact`, `endswith`, `iendswith`, `regex`, `iregex`
- [ ] JSON lookups: `has_key`, `has_keys`, `has_any_keys`, JSON containment
- [ ] `refresh_from_db()` — reload from database
- [ ] Additional delete handlers: SET_NULL, SET_DEFAULT, PROTECT, DO_NOTHING
- [ ] `bulk_update()` — batch field updates
- [ ] `none()` — empty QuerySet

### Tier 2 — Important for Real-World Use

- [ ] Window functions (Rank, RowNumber, Lag, Lead, etc.)
- [ ] Database functions (Coalesce, Greatest, Least, NullIf, Lower, Upper, Concat, Now)
- [ ] `union()` / `intersection()` / `difference()` — set operations
- [ ] `explain()` — query plan output
- [ ] `in_bulk()` — batch PK lookup
- [ ] `only()` / `defer()` — partial field loading (compile-time safe)
- [ ] `reverse()` — reverse ordering
- [ ] `earliest()` / `latest()` — convenience ordering
- [ ] OrderBy NULLS FIRST/LAST
- [ ] Postgres-native aggregates: ArrayAgg, JsonAgg, StringAgg (first-class, not contrib)
- [ ] `DecimalField` / `UUIDField` (non-PK)
- [ ] `GeneratedField` — computed columns
- [ ] `iterator()` / streaming — memory-efficient large result sets
- [ ] Trait-based model hooks (replace Django signals)
- [ ] Admin console: Maahi (Dioxus full-stack via `djogi-maahi`) — see Phase 10

### Tier 3 — Nice to Have

- [ ] Abstract model composition (shared field groups)
- [ ] Proxy models
- [ ] Math functions (Abs, Ceil, Floor, Round, etc.)
- [ ] Text functions (Trim, Replace, Substr, etc.)
- [ ] DateTime functions (Extract, Trunc, Now)
- [ ] Aggregate FILTER clause (Postgres-native)
- [ ] `contains(obj)` — instance membership check
- [ ] `bulk_create` with `update_conflicts` (upsert)
- [ ] Model-wide validation hooks (`clean()` / `full_clean()`)

---

## 12. Migration System Gap Analysis

### What Djogi's Current Spec Covers

- Build-time drift detection via `build.rs`
- Auto-generation of up/down SQL pairs
- Schema snapshot (`schema_snapshot.json`) as source of truth
- library `djogi::migrate::apply_plan` to apply; the `djogi migrations apply` CLI dispatcher is deferred
- `--allow-destructive` for DROP operations
- `--fake` for marking applied without running
- Rollback (last migration)
- Migration folder as git submodule

### What Django Does That Djogi Should Consider

#### Must Add

| Capability | Django | Djogi Recommendation |
|---|---|---|
| **Rename detection (fields)** | Compares field structure, asks user interactively | Auto-detect via `#[field(renamed_from)]` (already in spec). Also support CLI prompt: `djogi makemigrations --interactive` |
| **Rename detection (models/tables)** | Compares field structure across old/new models | Support `#[model(renamed_from = "old_table")]` annotation |
| **Data migrations** | `RunPython` / `RunSQL` for data transforms | Support `-- djogi:data` marker in SQL migrations for hand-written data transforms. Or separate `data_migrations/` with Rhai scripts that run in the shell environment |
| **NOT NULL addition handling** | Prompts for one-off default when adding NOT NULL column to existing table | `djogi makemigrations` should detect this and either prompt or emit `DEFAULT <value>` in the ALTER |
| **Migration dependencies** | Cross-app ordering via FK references | Djogi: migrations are app-scoped. Cross-app FKs create ordering dependencies. Track in `schema_snapshot.json`. |
| **Merge migrations** | When two branches add migrations, create a merge node | Detect conflicting migration numbers and prompt for merge |
| **Dry run** | `--dry-run` shows SQL without writing | Already in spec. Confirm it works for both auto-generated and manual migrations. |
| **SQL collection** | `sqlmigrate` command shows SQL for a migration | Add `djogi migrate show 0005` to display SQL without running |

#### Where Djogi Can Do Better

| Django Weakness | Djogi Opportunity |
|---|---|
| Migrations are Python files with operation objects — heavy, hard to review | Djogi: **plain SQL files**. Readable, editable, reviewable. No framework-specific format. |
| Django's autodetector is ~2000 lines of Python running 27 detection steps | Djogi: **build-time Rust code** comparing typed `ModelDescriptor` structs. Faster, more reliable, no runtime reflection. |
| Squashing is complex (replacement graph, partial application detection) | Djogi: **SQL files can be manually concatenated**. Or provide `djogi migrate squash 0001..0010` that merges SQL files. Simpler than Django's replacement system. |
| `RunPython` data migrations can't be represented as SQL | Djogi: data migrations are **Rhai scripts** or **raw SQL** — both are inspectable, no opaque Python. |
| Django must support 4+ database backends in migrations | Djogi: **Postgres-only SQL**. No backend abstraction. Generated SQL uses Postgres-specific features directly (transactional DDL, `IF NOT EXISTS`, `CONCURRENTLY`, etc.) |
| Django's `fake_initial` uses runtime introspection to detect existing tables | Djogi: can use Postgres `information_schema` queries at migrate time for the same purpose, more reliably. |
| Operation reduction/optimization is a complex pairwise algorithm | Djogi: since migrations are SQL, optimization is simpler — the differ generates optimal SQL directly from the diff rather than generating operations that need post-hoc optimization. |

#### Postgres-Specific Migration Advantages

These are things Djogi can do because it's Postgres-only:

| Feature | Description |
|---|---|
| **Transactional DDL** | Every migration runs in a transaction. Failure = clean rollback. Django can't guarantee this on MySQL. |
| **`CREATE INDEX CONCURRENTLY`** | Non-blocking index creation. Django supports it via `AddIndex(concurrently=True)` but it's a special case. Djogi should make it the default or at least trivially opt-in. |
| **`ALTER TABLE ... ADD COLUMN ... DEFAULT` (fast)** | Postgres 11+ adds NOT NULL columns with defaults without rewriting the table. Djogi should leverage this. |
| **Advisory locks** | Already in Djogi spec for preventing concurrent migration runners. |
| **`IF NOT EXISTS` / `IF EXISTS`** | Idempotent DDL. Useful for `fake_initial` equivalent. |
| **`pg_dump` / `pg_restore`** | Schema snapshots can use actual Postgres introspection instead of a JSON file. |

#### Data Migration Design

Django has `RunPython(code, reverse_code)` for data migrations. Djogi needs an equivalent. Options:

**Option A — SQL-only data migrations:**
```sql
-- migrations/0005_backfill_slugs_up.sql
-- djogi:data
UPDATE vehicles SET slug = lower(replace(make || '-' || model_name, ' ', '-'))
WHERE slug IS NULL;
```

**Option B — Rhai script data migrations:**
```rhai
// migrations/0005_backfill_slugs.rhai
// Runs in the shell environment with full model API
let vehicles = Vehicle::objects()
    .filter_struct(VehicleFilter::new().slug(IsNull()))
    .fetch_all();

for car in vehicles {
    car.slug = car.make.to_lower() + "-" + car.model_name.to_lower().replace(" ", "-");
    car.save();
}
```

**Recommendation:** Support both. SQL for simple transforms, Rhai for complex logic that benefits from the model API. Rhai scripts are inspectable (unlike Python bytecode) and run in the same environment the developer already knows from the shell.

---

## 13. Admin Renderer — Same Resolution as §9

Earlier draft analysis recommended HTMX + Askama over Dioxus for the admin renderer. The decision was reversed in favor of Dioxus full-stack during Phase 10 design — see §9 above and the Maahi spec at [`docs/spec/maahi/`](./maahi/index.md). The carve-out lives in `djogi-maahi`; per-adopter dep weight is bounded by the optional-dep behind the `admin` feature flag.

---

## 14. Summary Priority Tiers (Updated)

### Tier 1 — Must Have for ORM Functionality Parity

- [ ] `exclude()` — negative filtering
- [ ] `annotate()` + aggregates (Count, Sum, Avg, Max, Min)
- [ ] `aggregate()` — terminal aggregate
- [ ] Field expressions (F-equivalent) — typed field references in updates/annotations
- [ ] `update()` on QuerySet — bulk update without loading
- [ ] `delete()` on QuerySet — bulk delete without loading
- [ ] `select_related()` — JOIN-based eager loading
- [ ] `exists()` — optimized existence check
- [ ] `distinct()` — with Postgres DISTINCT ON
- [ ] `get_or_create()` / `update_or_create()` — atomic upsert
- [ ] `select_for_update()` — row locking
- [ ] `values()` / `select()` — partial column selection with typed returns
- [ ] `Subquery` / `Exists` / `OuterRef` — correlated subqueries
- [ ] `Case` / `When` — conditional expressions
- [ ] Additional lookups: `iexact`, `endswith`, `iendswith`, `regex`, `iregex`
- [ ] JSON lookups: `has_key`, `has_keys`, `has_any_keys`, JSON containment
- [ ] `refresh_from_db()` — reload from database
- [ ] Additional delete handlers: SET_NULL, SET_DEFAULT, PROTECT, DO_NOTHING
- [ ] `bulk_update()` — batch field updates
- [ ] `none()` — empty QuerySet
- [ ] Migration: rename detection (model + field) with annotations
- [ ] Migration: NOT NULL addition handling (prompt or default)
- [ ] Migration: data migration support (SQL + Rhai)
- [ ] Trait-based model hooks (replace Django signals)

### Tier 2 — Important for Real-World Use

- [ ] Window functions (Rank, RowNumber, Lag, Lead, etc.)
- [ ] Database functions (Coalesce, Greatest, Least, NullIf, Lower, Upper, Concat, Now)
- [ ] `union()` / `intersection()` / `difference()` — set operations
- [ ] `explain()` — query plan output
- [ ] `in_bulk()` — batch PK lookup
- [ ] `only()` / `defer()` — partial field loading (compile-time safe)
- [ ] `reverse()` — reverse ordering
- [ ] `earliest()` / `latest()` — convenience ordering
- [ ] OrderBy NULLS FIRST/LAST
- [ ] Postgres-native aggregates: ArrayAgg, JsonAgg, StringAgg (first-class)
- [ ] `DecimalField` / `UUIDField` (non-PK)
- [ ] `GeneratedField` — computed columns
- [ ] `iterator()` / streaming — memory-efficient large result sets
- [ ] Admin console: Maahi (Dioxus full-stack via `djogi-maahi`) — see Phase 10
- [ ] Migration: `CREATE INDEX CONCURRENTLY` support
- [ ] Migration: merge migration support
- [ ] Migration: `djogi migrate show` (display SQL)
- [ ] Migration: squash support

### Tier 3 — Nice to Have

- [ ] Abstract model composition (shared field groups)
- [ ] Proxy models
- [ ] Math functions (Abs, Ceil, Floor, Round, etc.)
- [ ] Text functions (Trim, Replace, Substr, etc.)
- [ ] DateTime functions (Extract, Trunc, Now)
- [ ] Aggregate FILTER clause (Postgres-native)
- [ ] `contains(obj)` — instance membership check
- [ ] `bulk_create` with `update_conflicts` (upsert)
- [ ] Model-wide validation hooks (`clean()` / `full_clean()`)
- [ ] Migration: interactive mode for ambiguous changes

---

## 15. Djogi-Only Advantages (Things Django Cannot Do)

### Computed Properties That Are Queryable

In Django, a `@property` on a model is purely Python-side — it can't be used in `.filter()` or `.order_by()`. The ORM doesn't know how to translate it to SQL. The workaround is `annotate()` with database expressions, but the developer manually duplicates the logic.

In Djogi, because `#[derive(Model)]` has full access to the AST at compile time, a computed property can carry **both** the Rust getter and the SQL expression:

```rust
#[derive(Model)]
pub struct Vehicle {
    pub base_price: i32,
    pub tax_rate: f64,

    #[computed(sql = "base_price * (1.0 + tax_rate)")]
    pub total_price: f64,  // getter in Rust, queryable in SQL
}

// Works as a Rust getter
let price = car.total_price;

// ALSO works in queries — the macro injects the SQL expression
Vehicle::objects()
    .filter(|f| f.total_price.gte(50_000))
    .order_by(|f| f.total_price.desc())
    .fetch_all(&mut ctx).await?;
```

The proc macro generates:
- A Rust method that computes `self.base_price as f64 * (1.0 + self.tax_rate)` for in-memory access
- A SQL expression `base_price * (1.0 + tax_rate)` injected into SELECT/WHERE clauses when used in queries

This is genuinely impossible in Django because Python properties are opaque to the ORM. Rust's proc macro sees both the Rust logic and the SQL annotation at compile time. Related to Postgres `GENERATED` columns but more flexible — the computed field doesn't have to be stored in the DB.

### Typed Tuples from `values()` / `select()`

Django's `values()` returns untyped dicts. Djogi returns **compile-time typed tuples**.

### Compile-Time Field Validation

Django's `F('nonexistent_field')` fails at runtime. Djogi's `|f| f.nonexistent_field` fails at compile time.

### Phantom Types for Deferred Fields

Django's `only()`/`defer()` defers to runtime — accessing a deferred field triggers an implicit query. Djogi can make partial models a distinct type at the compiler level.

### Single-Backend SQL Generation

Django generates SQL through 4+ backend dispatch layers. Djogi generates Postgres SQL directly — one codepath, no abstraction tax, direct access to JSONB operators, DISTINCT ON, advisory locks, transactional DDL, etc.

### Postgres-Native Features as First-Class Citizens

Everything Django hides in `contrib.postgres` is first-class in Djogi:
- `Array<T>` — native Postgres arrays with full operator support
- Full-text search — SearchVector/SearchQuery/SearchRank/trigrams
- `Range<T>` — Postgres range types with all 8 operators
- ExclusionConstraint — no equivalent in other DBs
- GIN/GiST/BRIN indexes — with tuning parameters
- ArrayAgg / JSONBAgg / StringAgg — natively, not contrib
- `CREATE INDEX CONCURRENTLY` — zero-downtime index creation
- `NOT VALID` + `VALIDATE CONSTRAINT` — zero-downtime constraint addition

---

## 16. Additional Items from Deep Dive

### Bulk Upsert (`bulk_create` with `update_conflicts`)

Django 4.1+ supports upsert via `bulk_create(update_conflicts=True, update_fields=[...], unique_fields=[...])`. This maps to Postgres `INSERT ... ON CONFLICT (unique_fields) DO UPDATE SET ...`.

Djogi must support this as a first-class pattern:

```rust
Vehicle::bulk_upsert(&mut ctx, vehicles, BulkUpsert {
    conflict_fields: |f| (f.vin,),
    update_fields: |f| (f.gas_fill, f.active),
}).await?;
```

This is Tier 1 — bulk upsert is extremely common in data pipelines, imports, and sync workflows.

### Transaction System

Djogi needs:
- `atomic()` — context wrapper via `tokio-postgres` transactions, supports nesting via savepoints
- `on_commit()` — callbacks that fire only after outermost transaction commits (for emails, events, cache invalidation). Cleared on rollback.
- `durable` flag — guarantee real top-level transaction, not a savepoint
- Savepoint-aware callback tracking — rollback discards callbacks registered within that savepoint

### Abstract Model Composition

Promote from Tier 3 to **Tier 2**. Enterprise use cases:
- `Auditable` — shared `created_at`, `updated_at`, `created_by` fields
- `SoftDeletable` — `deleted_at` + default filter excluding deleted records
- `TenantScoped` — `tenant_id` FK + scoped default filter
- `Orderable` — `position` field with index

Implement via derive macros that inject fields:
```rust
#[derive(Auditable, SoftDeletable, Model)]
pub struct Vehicle { ... }
```

### Proxy Models

Promote from Tier 3 to **Tier 2**. Enterprise use cases:
- Status-based views (`ActiveUser`, `InactiveUser`)
- Role-based admin registrations with different permissions
- Behavioral variants (different ordering, different methods)

```rust
#[derive(Model)]
#[model(proxy_for = "Vehicle", default_order = ["-created_at"])]
pub struct RecentVehicle;
```

### Explicitly Out of Scope

- Multi-database routing (`using()`) — single Postgres target
- Multi-table inheritance — too much implicit magic
- File/Image fields — Djogi is an ORM, not file storage
- GIS fields — separate concern (PostGIS extension could be a future add-on)
- CompositePrimaryKey — even Django 6.0 doesn't support FK to composite PK
- Dynamic signal registry — use trait-based hooks instead
