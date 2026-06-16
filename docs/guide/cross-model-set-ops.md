> [Back to Guides](./index.md) · [Back to README](../../README.md)

# Cross-Model Set Operations

Djogi provides typed `UNION`, `UNION ALL`, `INTERSECT`, and `EXCEPT` operations
between querysets whose `Model` types differ but whose column shapes are
compatible with a common decode target.

---

## When to reach for cross-model set ops vs same-model set ops

Use **same-model set ops** (`QuerySet::union`, `intersect`, `except`) when both
arms share the same `Model` type. They are method calls on `QuerySet<T>` and
infer the result type from the model itself:

```rust
// Same model — both arms are QuerySet<Dog>
let adopted = Dog::objects().filter(|f| f.status().eq(Status::Adopted));
let fostered = Dog::objects().filter(|f| f.status().eq(Status::Fostered));
let rows: Vec<Dog> = adopted.union(fostered).fetch_all(&mut ctx).await?;
```

Use **cross-model set ops** when the arms come from different `Model` types but
you want to decode the combined result into a single row type. Common cases
include audit events, unified feeds, or any scenario where multiple entity
tables project into one output shape:

```rust
// Different models — LoginEvent and ContentEdit, decoded as Activity
let logins = LoginEvent::objects().filter(|f| f.created_at().gt(last_hour()));
let edits  = ContentEdit::objects().filter(|f| f.created_at().gt(last_hour()));
let activities: Vec<Activity> = union_as::<Activity, _, _>(logins, edits)
    .fetch_all(&mut ctx)
    .await?;
```

---

## The decode target `R`

Cross-model set ops require an explicit decode target type parameter `R:
FromPgRow`. This is the Rust type that Postgres rows will be decoded into. It
is specified as a turbofish on the free constructor:

```rust
union_as::<Activity, _, _>(logins, edits)
//        ^^^^^^^^  decode target R
```

Rust needs `R` because the two arms may have different `Model` types, so the
result type cannot be inferred from either arm alone. The decode target is
independent of both `LeftModel` and `RightModel`; it only needs to match the
column shapes produced by the arm SELECT projections at runtime. Column
mismatches surface as Postgres decode errors when the terminal executes.

---

## Arm types

Cross-model set ops accept any type implementing [`IntoCrossArm<R>`](djogi::query::cross_set_op::IntoCrossArm):

| Arm type | Source | Notes |
|----------|--------|-------|
| `QuerySet<M>` | Any model's queryset | Subject to arm restrictions (see below) |
| `VisageQuerySet<V>` | Visage queryset | Bypasses QuerySet arm restrictions; enables cross-schema projection |

### Arm requirements for `QuerySet` arms

Each `QuerySet` arm must be **clean**: no prefetch, `select_related`, lock, or
cache bindings. These are validated at SQL-build time and return
`DjogiError::SetOpArmInvalid` when present:

```rust
// REJECTED at terminal execution — arm carries a prefetch registration
let bad = union_as::<Activity, _, _>(
    LoginEvent::objects().prefetch(|f| f.user()),
    ContentEdit::objects(),
).fetch_all(&mut ctx).await;
// Err(DjogiError::SetOpArmInvalid { side: "left", reason: "...", .. })
```

The same restrictions apply to `.select_related()`, `.select_for_update()`,
`.nowait()`, `.skip_locked()`, and `.cache()`.

Visage arms (`VisageQuerySet<V>`) bypass these checks because visages are
already narrowed SELECT projections that cannot carry joins or locks.

---

## Outer modifiers

After the set operator combines the two arms, you can apply ordering, limiting,
and pagination to the **combined result**:

```rust
union_as::<Activity, _, _>(logins, edits)
    .order_by("created_at", OuterOrder::Desc)  // sort combined result
    .limit(50)                                  // cap at 50 rows
    .offset(10)                                 // skip first 10
```

Each arm retains its own per-arm `ORDER BY` / `LIMIT` / `OFFSET` inside the
parenthesised subquery. The outer modifiers bind to the combined result after
the set operator.

### Outer ORDER BY column names

Outer `order_by` accepts a string column name, not a `FieldRef`. The column
must be:

- A valid ASCII identifier (letters, digits, underscore; starts with letter or `_`)
- Not in the framework-reserved `__djogi_` namespace

Invalid columns return `DjogiError::SetOpOuterOrderingInvalid` at SQL-build
time before any database round trip.

---

## Tenant reconciliation

When both arms use tenant-keyed models, terminal execution reconciles the
**intended tenants** before issuing any `SET LOCAL`:

1. Both arms report their intended tenant from the `DjogiContext` auth state.
2. If both carry a concrete tenant and they **differ**, the terminal returns
   `DjogiError::CrossModelSetOpTenantConflict` without modifying the connection's
   GUC state.
3. If at most one arm carries a concrete tenant, or both agree, execution
   proceeds with the shared tenant scope.

This reconciliation runs **before** any tenant wiring is fired, preventing the
GUC-poisoning defect where firing both arms' tenant wiring would overwrite the
connection state before the conflict is detected.

Untenantated models always report `None` as their intended tenant and are
compatible with any arm.

---

## Concrete examples

### UNION: unified activity feed from different entity tables

```rust
use djogi::prelude::*;

#[model(table = "login_events")]
#[derive(Debug, Clone)]
pub struct LoginEvent {
    pub user_id: HeerId,
    pub event_type: String,
    pub created_at: DateTime,
}

#[model(table = "content_edits")]
#[derive(Debug, Clone)]
pub struct ContentEdit {
    pub user_id: HeerId,
    pub event_type: String,
    pub created_at: DateTime,
}

/// Shared decode target — both tables expose compatible columns.
#[derive(Debug, Clone, FromPgRow)]
pub struct Activity {
    pub user_id: HeerId,
    pub event_type: String,
    pub created_at: DateTime,
}

// Build the feed
let logins = LoginEvent::objects()
    .filter(|f| f.created_at().gt(one_hour_ago()));
let edits  = ContentEdit::objects()
    .filter(|f| f.created_at().gt(one_hour_ago()));

let feed: Vec<Activity> = union_as::<Activity, _, _>(logins, edits)
    .order_by("created_at", OuterOrder::Desc)
    .limit(100)
    .fetch_all(&mut ctx)
    .await?;
```

### INTERSECT: items present in both warehouse tables

```rust
let wh_east  = WarehouseEast::objects().filter(|f| f.stocked().eq(true));
let wh_west  = WarehouseWest::objects().filter(|f| f.stocked().eq(true));

let in_both: Vec<Inventory> = intersect_as::<Inventory, _, _>(wh_east, wh_west)
    .fetch_all(&mut ctx)
    .await?;
```

### EXCEPT: set difference

```rust
// Users who have profiles but have never logged in via SSO.
let with_profile = UserProfile::objects();
let sso_logins   = SsoLoginRecord::objects();

let no_sso: Vec<UserSummary> = except_as::<UserSummary, _, _>(with_profile, sso_logins)
    .fetch_all(&mut ctx)
    .await?;
```

---

## Visage arms and cross-schema projection

Visage arms (`VisageQuerySet<V>`) enable a powerful pattern: two different
backend models can project through the same public `DjogiVisage` type, and you
can union their visage querysets directly. The visage's narrowed SELECT
projection becomes the arm shape:

```rust
// Two different backend models, same public visage projection
let users = User::visage_public()
    .objects()
    .filter(|f| f.created_at().gt(thirty_days_ago()));
let vendors = Vendor::visage_public()
    .objects()
    .filter(|f| f.created_at().gt(thirty_days_ago()));

// Union into UserPublic rows — both visages project the same columns
let all: Vec<UserPublic> = union_as::<UserPublic, _, _>(users, vendors)
    .fetch_all(&mut ctx)
    .await?;
```

This is useful for audience-facing queries that need to combine data from
different backend tables through a shared public projection. See the
[visages guide](./visages.md) for how `DjogiVisage` types are generated.

---

## Terminals

| Terminal | Return type | Notes |
|----------|-------------|-------|
| `fetch_all(&mut ctx)` | `Vec<R>` | Collects all result rows |
| `first(&mut ctx)` | `Option<R>` | Executes with `LIMIT 1` |
| `count(&mut ctx)` | `i64` | Wraps the set op in `SELECT COUNT(*) FROM (...) AS sub`; strips outer ORDER BY / LIMIT / OFFSET |

All terminals run arm validation, outer-ordering validation, and tenant
reconciliation before emitting SQL. Errors return before any database round
trip when the query shape is invalid.
