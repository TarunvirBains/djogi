> [Back to Guides](./index.md) · [Back to README](../../README.md)

Spec: [`docs/spec/models.md`](../spec/models.md) — Phase 5 dirty-tracking additions.

# Tracked Fields

`Tracked<T>` is a value wrapper you declare directly in a model struct to opt
a field into explicit dirty tracking. You write `pub email: Tracked<String>`;
the framework records whether the value has been mutated and, at `save()` time,
only includes dirty fields in the UPDATE SET list. Fields declared with a plain
type (not wrapped in `Tracked<T>`) are always included in every `save()`,
matching the Phase 4 baseline behavior.

---

## Contract

- You declare `pub field_name: Tracked<T>` in the struct. No attribute is
  needed — the type is the activation.
- A freshly constructed `Tracked<T>` (via `Tracked::new(v)`) starts **clean**.
  A row loaded from the database also starts clean — `FromSql` always constructs
  with `dirty = false`.
- Any write through `DerefMut` (including `+=`, `.push_str(...)`, or a direct
  assignment via `*field = value`) flips `dirty` to `true`.
- Read-only dereferences (`Deref`) never touch the dirty flag.
- `save()` reads `field.is_dirty()` on each `Tracked<T>` field and omits clean
  fields from the UPDATE SET list. After `RETURNING *` rehydration completes,
  `save()` resets every `Tracked<T>` field to clean.
- `Tracked<T>` serializes and deserializes transparently — serde sees the inner
  `T`. A value round-tripped through JSON still starts clean on the other side.

---

## Example

```rust
use djogi::prelude::*;
use djogi::tracked::Tracked;

#[model(table = "users")]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct User {
    pub username: String,
    // Only this field participates in dirty tracking.
    pub email: Tracked<String>,
}

async fn example(pool: &DjogiPool) -> Result<(), DjogiError> {
    let mut ctx = DjogiContext::from_pool(pool.clone());

    // Create a new user. Both fields are written on INSERT.
    let mut user = User::create(&mut ctx, User {
        username: "alice".to_string(),
        email: Tracked::new("alice@example.com".to_string()),
        ..Default::default()
    }).await?;

    // First save: email has not changed since load — it is clean, so it is
    // OMITTED from the SET list. But username is a plain String, so it is
    // ALWAYS written. The emitted SQL is:
    //   UPDATE users SET username = $1, updated_at = now() WHERE id = $2
    user.save(&mut ctx).await?;

    // Mutating through DerefMut marks email dirty.
    *user.email = "alice@new.example.com".to_string();
    assert!(user.email.is_dirty());

    // Second save: email is now dirty, so it joins username in the SET list.
    // Emits: UPDATE users SET username = $1, email = $2, updated_at = now() WHERE id = $3
    // username is a plain String — always included. email is Tracked and dirty — included.
    user.save(&mut ctx).await?;

    // After save(), email is clean again.
    assert!(!user.email.is_dirty());

    Ok(())
}
```

---

## Common Patterns

### Opting only high-churn fields into dirty tracking

You do not have to wrap every field. Wrap fields that change frequently
(user-entered data, counters, status strings) and leave stable fields
(foreign keys, created-at, enums set once) as plain types. The framework
includes plain-type fields in every `save()` regardless.

```rust
#[model(table = "documents")]
pub struct Document {
    pub title: String,           // plain — always written on save()
    pub body: Tracked<String>,   // only written when the user edits
    pub author_id: ForeignKey<User>,  // plain — set once, never changes
}
```

### Marking clean after a manual rehydrate

If you load values into a `Tracked<T>` field through some path other than
`FromSql` (for example, populating from a web form before the first INSERT),
and you want to avoid treating those initial values as a pending write, call
`.mark_clean()` explicitly:

```rust
let mut doc = Document {
    body: Tracked::new(form.body.clone()),
    // ... other fields
};
doc.body.mark_clean();   // not dirty — equivalent to a freshly loaded row
```

### Composing with `#[field(version)]` optimistic locking

`Tracked<T>` and `#[field(version)]` work independently on the same model.
The version field is always included in the WHERE predicate and incremented in
the SET list regardless of dirty tracking. `Tracked<T>` fields obey their own
dirty rules on top.

```rust
#[model(table = "accounts")]
pub struct Account {
    pub balance: Tracked<i64>,
    #[field(version)]
    pub revision: i32,   // always checked + incremented by save()
}
```

---

## Escape Hatch

### Manual dirty flag control

`mark_clean()` is `pub` (though it is primarily intended for macro-emitted
save bodies). Calling it before `save()` prevents a field from being written
even if it was mutated:

```rust
*user.email = "changed@example.com".to_string();
user.email.mark_clean();   // suppress this mutation from the next save()
user.save(&mut ctx).await?;  // email is NOT written
```

Use this sparingly — silently suppressing a mutation is usually a sign that
the application logic should be restructured.

### Bypassing the ORM entirely

For bulk writes or when the dirty-tracking surface does not cover your use
case, `ctx.raw_execute(sql, &[...])` accepts arbitrary SQL with positional
`$n` parameters and is transaction-aware. Like every raw escape, it is
reachable only when the enclosing item is decorated with
`#[djogi::deliberately_bypass_convention_with_raw_sql]` and the
adjacent `// JUSTIFICATION (djogi#<n>): ...` comment names the
typed-surface gap (see [Raw SQL escape hatches](../spec/raw-sql-escape-hatches.md)):

```rust
use djogi::prelude::*;

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#234): targeted bulk write bypasses the dirty-tracking pipeline by design.
async fn force_email(ctx: &mut DjogiContext, user_id: HeerId) -> djogi::Result<u64> {
    ctx.raw_execute(
        "UPDATE users SET email = $1 WHERE id = $2",
        &[&"alice@example.com", &user_id],
    ).await
}
```

See the [agent guide](./agent-guide.md) for the full raw-query surface.
