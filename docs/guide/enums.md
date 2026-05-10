> [Back to Guides](./index.md) · [Back to README](../../ReadMe.MD)

Spec: [`docs/spec/models.md`](../spec/models.md) — Phase 5 enum type support.

# Enums

`#[derive(DjogiEnum)]` turns a plain Rust enum into a Postgres-native enum
type. One derive emits both the `postgres_types` codec (so the value round-trips
as a Postgres TEXT/ENUM wire string) and an `inventory::submit!` of an
`EnumDescriptor` (so the migration projection can emit
`CREATE TYPE ... AS ENUM (...)` alongside your schema). It also implements
Djogi's SQL-type bridge so model fields of that enum project to the named
Postgres enum type. Single source of truth — add a variant once, and the
codec, descriptor, and model projection stay in sync.

---

## Contract

- You derive `DjogiEnum` on a plain C-like enum (unit variants only — no tuple
  or struct variants).
- Default variant-to-string mapping is `snake_case`. You override the mapping
  at the enum level with `#[djogi_enum(rename_all = "...")]` or per variant
  with `#[djogi_enum_variant(name = "...")]`.
- **`#[djogi_enum(name = "...")]` is required.** There is no default Postgres
  type name — omitting it is a compile error. Name the Postgres enum type
  explicitly every time.
- `FromSql` returns `EnumDecodeError` when the wire string does not match any
  known variant — no silent unknown-variant handling.
- DDL (`CREATE TYPE ... AS ENUM (...)`) is emitted by the migration projection
  from `EnumDescriptor` metadata before tables reference the enum type. Ordinary
  `#[djogi_test(sync_models = [...])]` tests do not need hand-written enum DDL.

---

## Example

```rust
use djogi::prelude::*;
use djogi_macros::DjogiEnum;

// Default rename: Active => "active", InReview => "in_review", Retired => "retired".
#[derive(DjogiEnum, Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[djogi_enum(name = "post_status")]
pub enum PostStatus {
    Active,
    InReview,
    Retired,
}

#[model(table = "posts")]
#[derive(Debug, Clone)]
pub struct Post {
    pub title: String,
    pub status: PostStatus,
}

async fn example(pool: &DjogiPool) -> Result<(), DjogiError> {
    let mut ctx = DjogiContext::from_pool(pool.clone());

    let post = Post::create(&mut ctx, Post {
        id: HeerId::ZERO,
        created_at: Default::default(),
        updated_at: Default::default(),
        title: "Getting started with Djogi".to_string(),
        status: PostStatus::Active,
    }).await?;

    // Filter by enum value.
    let active_posts = Post::objects()
        .filter(|f| f.status().eq(PostStatus::Active))
        .fetch_all(&mut ctx).await?;

    assert_eq!(active_posts[0].status, PostStatus::Active);
    Ok(())
}
```

---

## Common Patterns

### Renaming all variants at once

The `rename_all` attribute accepts `"snake_case"` (the default), `"SCREAMING_SNAKE_CASE"`,
`"camelCase"`, `"PascalCase"`, and `"lowercase"`. Use `"SCREAMING_SNAKE_CASE"` when
matching an existing Postgres enum whose variants are uppercase:

```rust
#[derive(DjogiEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[djogi_enum(name = "severity_level", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Severity {
    Low,       // => "LOW"
    Medium,    // => "MEDIUM"
    Critical,  // => "CRITICAL"
}
```

### Per-variant override

When one variant needs a name that does not follow the enum-level rule, use
`#[djogi_enum_variant(name = "...")]` on that variant alone:

```rust
#[derive(DjogiEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[djogi_enum(name = "vehicle_status")]
pub enum VehicleStatus {
    Active,           // => "active"
    InMaintenance,    // => "in_maintenance"
    #[djogi_enum_variant(name = "decommissioned")]
    Retired,          // => "decommissioned"  (overrides the default "retired")
}
```

### Matching an existing Postgres enum type

When your database already has a Postgres enum type (perhaps created by a
previous migration or an external tool), use `#[djogi_enum(name = "...")]` to
align the Rust name with the Postgres type name exactly. The codec matches on
wire strings; as long as the variant-to-string mapping matches what Postgres
stores, no migration is needed.

### Enums in JSONB schemas

`DjogiEnum` variants can appear inside a `Jsonb<T>` schema as long as the enum
also derives `serde::Serialize` and `serde::Deserialize`. Serde uses the
`rename_all` rule independently of the Postgres codec — they may differ if the
JSON consumer and the Postgres column have different naming conventions.

---

## Escape Hatch

If the typed enum surface does not fit your use case (for example, you need to
store arbitrary string values from an external system), declare the column as
`String` and validate at the application layer:

```rust
#[model(table = "events")]
pub struct Event {
    pub kind: String,   // unconstrained — any Postgres text value
}
```

`EnumDecodeError` from a `DjogiEnum` field is surfaced as a `DjogiError::Decode`
wrapping the underlying decode error message. You can match on it explicitly
or handle it with `is_transient()` (it is `false` — unknown enum variants are
not transient).
