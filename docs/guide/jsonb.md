> [Back to Guides](./index.md) · [Back to README](../../ReadMe.MD)

Spec: [`docs/spec/jsonb.md`](../spec/jsonb.md) — full JSONB schema field specification.

# JSONB Fields

`Jsonb<T>` wraps a Postgres `JSONB` column with a typed Rust schema. Every load
splits the stored JSON object into two halves: `data` (keys whose names match
fields in `T`) and `extra` (keys that `T` does not declare). Every `save()`
merges both halves back into a single JSON object so no key is ever silently
dropped — even keys added by a newer service version or a manual migration are
preserved across round-trips.

Phase 5 adds two query surfaces for filtering on JSONB subfields: a flat
`path::<V>("dot.path")` escape hatch (available now) and a
`#[derive(JsonbSchema)]` typed deep-path tree (Task 6).

---

## Contract

- `T` must implement `serde::Serialize` and `serde::Deserialize`.
- Unknown keys (present in the JSON, absent from `T`) land in `Jsonb::extra`
  as `serde_json::Value` entries. They are never dropped.
- `Jsonb::new(value)` constructs a fresh instance with an empty `extra` map.
- `ToSql` serializes `data` and `extra` directly with no built-in validation
  hook. Call `serde_json::to_value` or `validator::Validate::validate` yourself
  before `save()` if you want pre-write validation.
- `FromSql` constructs `Jsonb<T>` from the wire bytes. `ToSql` merges `data`
  and `extra` before encoding.

---

## Example

```rust
use djogi::prelude::*;
use djogi::jsonb::{Jsonb, UnknownFieldExt};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMeta {
    pub timezone: String,
    pub locale: String,
}

#[model(table = "users")]
#[derive(Debug, Clone)]
pub struct User {
    pub name: String,
    pub meta: Jsonb<UserMeta>,
}

async fn example(pool: &DjogiPool) -> Result<(), DjogiError> {
    let mut ctx = DjogiContext::from_pool(pool.clone());

    let user = User::create(&mut ctx, User {
        name: "Alice".to_string(),
        meta: Jsonb::new(UserMeta {
            timezone: "UTC".to_string(),
            locale: "en-US".to_string(),
        }),
        ..Default::default()
    }).await?;

    // Filter on a known JSONB subfield via the flat path escape hatch.
    let utc_users = User::objects()
        .filter(|f| f.meta().path::<String>("timezone").eq("UTC".to_string()))
        .fetch_all(&mut ctx).await?;
    // Emits: WHERE (meta->>'timezone') = $1

    // Access unknown fields that the current schema does not declare.
    if let Some(val) = user.meta.extra().get("experimental_flag") {
        let _ = val.try_as_bool();  // fallible — never implicit coercion
    }

    Ok(())
}
```

---

## Filtering on JSONB Subfields

### Shape 1 — Flat path escape hatch

`.path::<V>("dot.path")` accepts a dotted string at runtime and emits a `->` /
`->>` chain with a cast to the SQL type for `V`. Each segment must be a plain
ASCII identifier (letter or underscore first, then alphanumerics or underscores,
at most 63 bytes). The path is validated at construction time and **panics in
both debug and release** if any segment is invalid. Keep path strings as
compile-time literals — do not interpolate user input into path strings.

> **`&'static str` only.** The path argument is `&'static str` — a
> runtime-constructed `String` cannot be passed. Path keys must be
> literals so the JSONB traversal is fully fixed at compile time.

```rust
// Single-level
User::objects()
    .filter(|f| f.meta().path::<String>("locale").eq("en-US".to_string()))
    // WHERE (meta->>'locale') = $1

// Two-level nesting
Vehicle::objects()
    .filter(|f| f.specs().path::<i32>("engine.cylinders").gt(4))
    // WHERE (specs->'engine'->>'cylinders')::int > $1
```

Supported comparison methods on the returned `JsonbPathRef<M, V>`:
`eq`, `neq`, `gt`, `gte`, `lt`, `lte`, `in_list`, `not_in_list`,
`is_null`, `is_not_null`.

### Shape 2 — Typed deep path via `#[derive(JsonbSchema)]`

For schemas you query repeatedly, derive `JsonbSchema` on the inner type to get
a compile-checked path tree:

```rust
use djogi::prelude::*;

#[derive(JsonbSchema, Serialize, Deserialize)]
pub struct EngineSpec {
    pub cylinders: i32,
    pub displacement_cc: f32,
}

#[derive(JsonbSchema, Serialize, Deserialize)]
pub struct VehicleSpec {
    pub engine: EngineSpec,
    pub weight_kg: f32,
}

#[model(table = "vehicles")]
pub struct Vehicle {
    pub make: String,
    pub spec: Jsonb<VehicleSpec>,
}

// Typed path — compile-checked. Call .typed() to enter the path tree,
// then drill down with method calls (not field access).
Vehicle::objects()
    .filter(|f| f.spec().typed().engine().cylinders().gt(4))
    // WHERE (spec->'engine'->>'cylinders')::int > $1
```

The derive generates a `{T}Path<M>` struct with one method per field.
Nested struct fields return the nested schema's own path type; scalar
fields return `JsonbPathRef<M, V>` with the full comparison surface.
Both shapes emit identical SQL. The method-call style (`engine()`,
`cylinders()`) matches how `Fields` accessors work elsewhere in the API
and lets the derive emit visibility-aware accessors rather than raw
public fields.

**Rule of thumb:** use the flat path for one-off escapes or while a schema is
still being designed. Switch to `#[derive(JsonbSchema)]` once the schema
stabilizes — the compiler catches typos and type mismatches.

---

## Unknown Fields

```rust
// DB stored: { "timezone": "UTC", "locale": "en-US", "beta_features": ["editor-v2"] }
// Schema only declares: timezone, locale.

let user = User::get(&mut ctx, user_id).await?;

// "beta_features" landed in extra — access it:
if let Some(val) = user.meta.extra().get("beta_features") {
    let arr = val.try_as_array()?;
    println!("{} beta features", arr.len());
}

// Iterate all unknown keys:
for (key, val) in user.meta.extra() {
    println!("unknown: {key} = {val:?}");
}
```

`UnknownField` is `serde_json::Value`. The `UnknownFieldExt` trait adds
fallible typed accessors: `try_as_str`, `try_as_i64`, `try_as_f64`,
`try_as_bool`, `try_as_array`, `try_as_object`, `try_into_typed::<T>`.
All return `Result<_, UnknownFieldError>` — no implicit coercion.

---

## Common Patterns

### Honoring `#[serde(rename)]` on schema types

`Jsonb<T>` delegates to `T`'s serde implementation, so `#[serde(rename)]` and
`#[serde(rename_all)]` on the schema struct work exactly as expected. The keys
stored in the database follow the serde names, not the Rust field names.

```rust
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrontendMeta {
    pub dark_mode: bool,         // stored as "darkMode"
    pub items_per_page: u32,     // stored as "itemsPerPage"
}
```

When filtering with `.path::<V>("...")`, use the wire key name (the serde name),
not the Rust field name: `.path::<bool>("darkMode")`.

---

## Escape Hatch

For advanced JSONB predicates that `path()` does not cover — `?` key existence,
`@>` containment, `jsonb_array_elements`, recursive path operators — use
`ctx.raw_execute` or `ctx.raw_query` with hand-written SQL. The raw API is
djogi's `unsafe`-equivalent: every call site must decorate the enclosing item
with `#[djogi::deliberately_bypass_convention_with_raw_sql]` and pair it with
an adjacent `// JUSTIFICATION (djogi#<n>): ...` comment naming the
typed-surface gap (see [Raw SQL escape hatches](../spec/raw-sql-escape-hatches.md)).

```rust
use djogi::prelude::*;

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#234): JSONB `@>` containment is not yet exposed by the typed path API.
async fn users_in_locale(ctx: &mut DjogiContext) -> djogi::Result<Vec<User>> {
    let rows = ctx.raw_query::<User>(
        "SELECT * FROM users WHERE meta @> $1::jsonb",
        &[&serde_json::json!({ "locale": "en-US" })],
    ).await?;
    Ok(rows)
}
```

To bind a `Jsonb<T>` value as a parameter in a raw query, the simplest path is
to pass `&jsonb` directly — `Jsonb<T>` implements `ToSql` against `JSONB` and
round-trips the merged (typed + unknown) payload without extra code. If you
only need the typed portion, `serde_json::to_value(&jsonb.data)` is public
because `data` is a public field; the unknown-field map lives in a
crate-private `extra` field and is not accessible from user code. Use
`Jsonb<T>`'s `ToSql` or a fresh struct you control rather than reaching for
`extra` directly.

---

## `MirJzSON` — raw / unschemed JSONB columns

`Jsonb<T>` is the right tool when you own the JSON schema in Rust. When you
**don't** — payloads owned by an external API, document-shaped content whose
schema evolves faster than the model can track, content that intentionally
carries unknown keys at every level — reach for `MirJzSON`.

Spec: [`docs/spec/mirjzson-jsonb-integration.md`](../spec/mirjzson-jsonb-integration.md).
Issue: [#195](https://github.com/TarunvirBains/djogi/issues/195).

### When to use `MirJzSON` vs `Jsonb<T>`

| Pick | When |
|---|---|
| `Jsonb<T>` | You own the schema and want compile-checked field access. Unknown keys are preserved across save round-trips but not first-class queryable. |
| `MirJzSON` | The schema is the database's, not Rust's. The same raw-JSON predicate needs to run against PostgreSQL rows and Punnu-local cache entries. |

`MirJzSON` wraps a Sassi `JSahibON` portable JSON value — the same value model
the Punnu cache uses, so `f.payload().jsahibon().path("a.b").value::<i64>().gte(4)`
emits SQL against PostgreSQL **and** evaluates locally against Punnu cache rows
through the same Sassi truth tables. No double implementation; no drift.

### Construction

`MirJzSON` deliberately has **no** `Default` impl — every value must come from
one of the named construction routes:

```rust
use djogi::prelude::*;
use sassi::JSahibON;
use serde_json::json;

// From an already-portable Sassi value:
let mir1: MirJzSON = JSahibON::I64(42).into();

// From a `serde_json::Value` (fallible — rejects non-finite f64,
// out-of-range arbitrary-precision numbers):
let mir2: MirJzSON = serde_json::Value::try_into(json!({"a": 1, "b": "two"}))
    .or_else(|err: MirJzSONError| panic!("unsupported: {err}"))
    .unwrap();
// or with `TryFrom` directly:
let mir3 = MirJzSON::try_from(json!({"c": 3}))?;

// From the database — the Postgres FromSql codec is automatic.
```

Projection back to portable / JSON is named so the cache-boundary direction
is visible at call sites:

```rust
let portable: &sassi::JSahibON = mir3.as_jsahibon();   // borrow
let owned: sassi::JSahibON = mir3.into_jsahibon();     // consume
let v: serde_json::Value = mir3.into();                // total — every JSahibON projects
```

### Equality posture

`MirJzSON` is **not** `PartialEq` / `Eq` / `Hash` / `PartialOrd`. Whole-document
equality goes through the JSON predicate methods, not the root `eq()` surface:

```rust
// Compiles — uses Sassi's object equality (order-insensitive on objects,
// numeric-softening across I64/U64/F64).
Post::objects().filter(|f| {
    f.payload()
        .jsahibon()
        .eq_json(sassi::JSahibON::I64(42))
});

// Does NOT compile — the root `eq()` is intentionally absent on MirJzSON
// because Rust `PartialEq` would silently disagree with the JSahibON
// truth tables.
// Post::objects().filter(|f| f.payload().eq(some_mirjzson_value));
```

### Filtering on `MirJzSON` paths

The `.jsahibon()` builder is the v1 query surface. It mirrors the `Jsonb<T>`
flat-path API (`path("a.b")` for dotted plain identifiers) plus `.key(...)` /
`.path_segments([...])` for arbitrary keys (non-identifier strings, digits,
embedded dots — anything a JSON object key can legally hold):

```rust
use djogi::prelude::*;

#[model(table = "events")]
pub struct Event {
    pub kind: String,
    // Future spec — see [Model gating](#model-gating-pending-macro-surface) below.
    pub payload: MirJzSON,
}

// Plain dotted identifier path — same shape as `Jsonb<T>::path`:
Event::objects().filter(|f| {
    f.payload()
        .jsahibon()
        .path("engine.cylinders")
        .value::<i64>()
        .gte(4)
});
// SQL: CASE WHEN jsonb_typeof((payload #> $1)) = 'number' THEN
//        ((payload #> $2) #>> '{}'::text[])::numeric >= $3
//      ELSE FALSE END

// Arbitrary key (hyphen, digits, embedded dots) — use `.key(...)`:
Event::objects().filter(|f| {
    f.payload()
        .jsahibon()
        .key("content-type")
        .value::<String>()
        .eq("application/json".to_string())
});

// Multi-segment literal path including non-identifier segments:
Event::objects().filter(|f| {
    f.payload()
        .jsahibon()
        .path_segments(["a.b", "0", "cafe"])
        .exists()
});
```

The full predicate surface mirrors Sassi's typed builders:

- **Existence**: `exists()`, `missing()`, `is_json_null()`, `is_not_json_null()`.
- **Type tests**: `is_bool()`, `is_number()`, `is_string()`, `is_array()`,
  `is_object()` (or `is_type(JTypeKind::…)` for the parametric form).
- **Object keys**: `has_key(k)`, `has_any_key([…])`, `has_all_keys([…])` — all
  guard `jsonb_typeof = 'object'`.
- **Scalar comparison**: `value::<V>().eq(x)` / `neq` / `in_(vec)` /
  `not_in(vec)` for `V` in `{ i64, u64, f64, String, bool }`; plus `gt` /
  `gte` / `lt` / `lte` / `between(low, high)` for numeric `V` only (string
  ordering is intentionally absent — locale collation is out of scope).
- **Whole-value equality**: `eq_json(JSahibON)`, `neq_json(JSahibON)`.
- **Arrays**: `array_contains(JSahibON)`, `array_len_eq/gt/gte/lt/lte(usize)`.

Every predicate emits a **two-valued** SQL boolean — missing path, type
mismatch, and SQL NULL all return `FALSE` (except `missing()` which is `TRUE`
on the missing case). The leaves compose safely under `&`, `|`, `^`, `!`
without SQL NULL leaking out.

### Numeric correctness — `u64::MAX` works

Numeric scalar comparisons emit a `CASE WHEN jsonb_typeof = 'number' THEN …
ELSE FALSE END` shape that casts through Postgres's `numeric` type — never
through `as i64`. Operands of every numeric carrier bind through
`rust_decimal::Decimal` so the full `u64` range, including `u64::MAX`, compares
correctly:

```rust
Event::objects().filter(|f| {
    f.payload()
        .jsahibon()
        .path("counter")
        .value::<u64>()
        .eq(u64::MAX)
});
// Binds u64::MAX through Decimal, not `as i64`.
```

### `Option<MirJzSON>` columns

The optional case distinguishes `None` (column SQL NULL) from
`Some(MirJzSON(JSahibON::Null))` (column present, JSON `null`):

```rust
#[model(table = "events")]
pub struct Event {
    pub maybe_payload: Option<MirJzSON>,
}

// `missing()` is true only on SQL NULL.
Event::objects().filter(|f| f.maybe_payload().jsahibon().missing());

// `is_json_null()` requires the column to be present AND hold JSON `null`.
Event::objects().filter(|f| f.maybe_payload().jsahibon().is_json_null());
```

### Sassi cache-boundary projection — `Jsonb<T>::to_jsahibon()`

When you ship a `Jsonb<T>` model through a Sassi-backed cache (Punnu) that
expects `JSahibON` on the wire, project explicitly:

```rust
use djogi::jsonb::Jsonb;
use sassi::JSahibON;

let jsonb: Jsonb<UserMeta> = /* loaded from DB */;
let portable: JSahibON = jsonb
    .to_jsahibon()
    .expect("typed schema must round-trip through Sassi");
// → carries the merged `data` + unknown `extra` document.
```

The conversion is fallible because `T`'s `Serialize` impl could in principle
produce a `serde_json::Number` outside Sassi's carrier range (non-finite f64,
arbitrary-precision integers). The error surfaces through
[`MirJzSONError`](https://docs.rs/djogi/latest/djogi/jsonb/enum.MirJzSONError.html).

### `.explicit_pg_predicate().mirjzson()` — reserved

The `.explicit_pg_predicate().mirjzson()` route exposes a PostgreSQL-only entry
point reserved for future JSONB operators with no Sassi-local contract
(`@?` / `@@` JSONPath, GIN-specific shapes). **V1 exposes no predicate methods
on the returned type** — every JSON query in v1 flows through `.jsahibon()` so
it is both SQL-lowerable and Punnu-evaluable.

If you reach for `.mirjzson()` expecting v1 predicate methods, the compiler
will tell you the type has no such methods. Route through `.jsahibon()`
instead — that is the v1 contract.

### Trusted provenance

`DjogiField<M, MirJzSON>::jsahibon()` is the only entry point that produces
predicates Djogi accepts for SQL lowering. Raw Sassi builders
(`sassi::Field::new("payload", _).jsahibon()...`) build `BasicPredicate<T>`
values that Sassi can evaluate locally, but Djogi rejects them at the type
level — the `PortablePredicate<T>` wrapper that flows into `QuerySet::filter`
can only be minted by Djogi-internal field methods. There is no way to smuggle
a forged `LookupOp::Json` predicate past Djogi's identifier validator.

### Model gating — pending macro surface

The MirJzSON v1 spec calls for a per-field justification attribute:

```rust
#[mirjzson(justification = "payload schema is owned by upstream partner SDK")]
pub payload: MirJzSON,
```

The macro-side enforcement of this attribute is **not yet shipped** in the
initial MirJzSON slice. The runtime / SQL / type-safe surface is complete —
adopters can use `MirJzSON` and `Option<MirJzSON>` model fields today — and
the justification gate will land as a follow-up to issue #195. Until then,
add a one-line comment above each `MirJzSON` field explaining why the schema
is genuinely external; the future macro will accept the attribute without a
code-change at the field site (the attribute is additive, not breaking).

### Escape hatches

For PostgreSQL-specific JSONB operators not yet covered by Sassi's portable
contract (`@?` / `@@` JSONPath, GIN-specific shapes, recursive operators),
fall back to `ctx.raw_execute` / `ctx.raw_query` per the
[Raw SQL escape hatches](../spec/raw-sql-escape-hatches.md) convention. The
raw API is djogi's `unsafe`-equivalent — every call site decorates the
enclosing item with `#[djogi::deliberately_bypass_convention_with_raw_sql]`
and pairs it with an adjacent `// JUSTIFICATION (djogi#<n>): ...` comment
naming the typed-surface gap. File the issue against djogi (not your
application) — every reach for raw SQL signals a gap in djogi's typed surface.
