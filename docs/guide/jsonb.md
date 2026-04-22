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
        id: HeerId::placeholder(),
        created_at: Default::default(),
        updated_at: Default::default(),
        name: "Alice".to_string(),
        meta: Jsonb::new(UserMeta {
            timezone: "UTC".to_string(),
            locale: "en-US".to_string(),
        }),
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
use djogi_macros::JsonbSchema;

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

// Typed path — compile-checked. Call .typed() to enter the path tree.
Vehicle::objects()
    .filter(|f| f.spec().typed().engine.cylinders.gt(4))
    // WHERE (spec->'engine'->>'cylinders')::int > $1
```

The derive generates a `{T}Path<M>` struct. Nested struct fields return the
nested schema's own path struct. Scalar fields return `JsonbPathRef<M, V>`
with the full comparison surface. Both shapes emit identical SQL.

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
`ctx.raw_execute` or `ctx.raw_query` with hand-written SQL:

```rust
let rows = ctx.raw_query::<User>(
    "SELECT * FROM users WHERE meta @> $1::jsonb",
    &[&serde_json::json!({ "locale": "en-US" })],
).await?;
```

To bind a `Jsonb<T>` value as a parameter in a raw query, serialize it yourself:
`serde_json::to_value(&jsonb.data)` gives the typed portion; `jsonb.extra` is
public and holds the unknown fields as a `serde_json::Value` map. `Jsonb<T>`
implements `ToSql` directly, so passing `&jsonb` as a bind argument also works
wherever a `JSONB` parameter is expected.
