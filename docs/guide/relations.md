> [Back to README](../../ReadMe.MD) | [All Guides](./index.md)

# Relations

A relation is a typed link between two models. Djogi keeps every relation
explicit at the call site — there is no lazy loading, no stringly-typed path
syntax, and no implicit many-to-many junction table. If a relation touches
the database, the `.fetch(...)` / `.prefetch(...)` / `.select_related(...)`
verb makes that visible at the call site.

This document is a Phase 3 reference. For generated CRUD, model attributes,
and field types, see the [models guide](./models.md); for the `QuerySet`
builder consumed below, see the [queries guide](./queries.md); for roadmap
features (`GenericForeignKey`, typed `IN` on FK PK sets, multi-hop prefetch),
see [the relations roadmap](../roadmap/relations.md).

---

## Relation Types at a Glance

| Declaration | Purpose | Accessor shape |
|---|---|---|
| `pub owner_id: ForeignKey<Owner>` | Many-to-one | `.fetch(&mut ctx)` / `.resolved()` / prefetch via `{Model}Related` |
| `pub user_id: OneToOneField<User>` | One-to-one (unique on the FK column) | same surface as `ForeignKey<T>` |
| `reverse_one_to_many!(Parent, name -> Child by fk)` | Reverse of a `ForeignKey` | inherent method `parent.name(&mut ctx) -> Vec<Child>` |
| `reverse_one_to_one!(Parent, name -> Child by fk)` | Reverse of a `OneToOneField` | inherent method `parent.name(&mut ctx) -> Option<Child>` |
| `many_to_many!(Source, Target, through = …, …)` | M2M via an explicit junction model | trait impl + `source.relation(&mut ctx) -> Vec<Target>` |

Every relation type has a matching entry in `ModelDescriptor::relations` so
admin/shell/migration tooling can enumerate them without parsing the struct.

---

## Foreign Keys

Declare a foreign key with `ForeignKey<T>`:

```rust
use djogi::prelude::*;
use djogi::relation::ForeignKey;

#[model(table = "owners")]
#[derive(Debug, Clone)]
pub struct Owner {
    pub name: String,
}

#[model(table = "vehicles", no_default)]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub make: String,
    #[field(on_delete = "cascade")]
    pub owner_id: ForeignKey<Owner>,
}
```

`ForeignKey<T>` stores the target model's primary key **only** — not an
eagerly-loaded row. `sqlx::Encode` / `Decode` route it through
`T::Pk`, so the on-disk column matches the target's PK column type
(`BIGINT` for HeerId, `UUID` for RanjId, `INTEGER` for serial).

Use `Option<ForeignKey<T>>` for nullable FK columns — the migration
system emits the `NULL` constraint from the `Option<_>` wrapper, not
from `on_delete`.

### Fetching a single related row

```rust
let owner: Owner = vehicle.owner_id.fetch(&mut ctx).await?;
// SELECT * FROM owners WHERE id = $1 LIMIT 1
```

`fetch(&mut ctx)` runs one `SELECT` against the target table. It errors
with `DjogiError::NotFound` if the FK points at a row that no longer
exists (stale reference). The `ctx` argument is a `&mut DjogiContext`
— construct one via `DjogiContext::from_pool(pool)` for pool-backed use,
or receive it from an enclosing transaction scope. Use `.resolved()` to
access an already-prefetched row without issuing SQL:

```rust
match vehicle.owner_id.resolved() {
    Some(owner) => /* prefetch already hydrated this */,
    None => /* not loaded — call .fetch(...) or prefetch() upstream */,
}
```

### `on_delete` behaviour

Only valid on `ForeignKey<T>` / `OneToOneField<T>` fields. Recorded in
`FieldDescriptor::on_delete`; consumed by the Phase 6 migration DDL
emitter.

| Value | SQL |
|---|---|
| `"restrict"` (default) | `ON DELETE RESTRICT` |
| `"cascade"` | `ON DELETE CASCADE` |
| `"set_null"` | `ON DELETE SET NULL` (requires `Option<ForeignKey<T>>`) |
| `"set_default"` | `ON DELETE SET DEFAULT` |
| `"protect"` | `ON DELETE RESTRICT` (aliases `restrict` at the SQL layer but carries distinct intent in the descriptor for admin UIs) |
| `"do_nothing"` | `ON DELETE NO ACTION` |

Cascade is **opt-in** on every FK — bulk deletes must be explicit per edge.

---

## Eager Loading

Field access on `ForeignKey<T>` never issues SQL. The explicit verbs:

| Verb | Query shape | When to use |
|---|---|---|
| `prefetch(relation)` | `SELECT * FROM parents WHERE ...; SELECT * FROM children WHERE parent_id IN ($1, ...)` | Avoids row explosion — preferred when parent rows are wide or when one parent has many children |
| `select_related(relation)` | `SELECT parents.*, children.* FROM parents LEFT JOIN children ON ...` | Single round-trip — preferred for singular FK / O2O where each parent has at most one related row |

Both consume a `{Model}Related` entry emitted by the `#[model]` macro.
For a `Vehicle` model with a `ForeignKey<Owner>` on `owner_id`, the macro
emits `VehicleRelated::owner_id()`:

```rust
// Effective emission — do not write this by hand
pub struct VehicleRelated;

impl VehicleRelated {
    pub fn owner_id() -> RelationPath { /* ... */ }
}
```

`prefetch` — stitched rows come back via `fetch_all_prefetched`, not
`fetch_all`. The plain `.fetch_all(&mut ctx)` terminal ignores any
registered prefetch paths (so Phase 2 call sites stay source-stable);
reach for the `_prefetched` terminal when you want the stitched output:

```rust
let rows: Vec<PrefetchedRow<Vehicle>> = Vehicle::objects()
    .prefetch(VehicleRelated::owner_id())
    .fetch_all_prefetched(&mut ctx)
    .await?;
// Query 1: SELECT * FROM vehicles
// Query 2: SELECT * FROM owners WHERE id IN ($1, $2, ...)
// Then row.get(VehicleRelated::owner_id()) returns Some(&owner) on each row.
```

`select_related` — stitched rows come back via `fetch_all_joined`, same
rationale:

```rust
let rows: Vec<JoinedRow<Vehicle>> = Vehicle::objects()
    .select_related(VehicleRelated::owner_id())
    .fetch_all_joined(&mut ctx)
    .await?;
// SELECT vehicles.*, owners.* FROM vehicles
// LEFT JOIN owners ON vehicles.owner_id = owners.id
```

Phase 3 supports **single-hop** prefetch / select_related. Chained multi-hop
(`.prefetch(VehicleRelated::owner_id().then(OwnerRelated::address_id()))`)
is on the roadmap — see [relations roadmap][relations-roadmap].

[relations-roadmap]: ../roadmap/relations.md

---

## One-to-One

Use `OneToOneField<T>` when the schema guarantees at most one child per
parent (UNIQUE on the FK column):

```rust
use djogi::relation::OneToOneField;

#[model(table = "users")]
#[derive(Debug, Clone)]
pub struct User {
    pub email: String,
}

#[model(table = "profiles", no_default)]
#[derive(Debug, Clone)]
pub struct Profile {
    pub bio: String,
    pub user_id: OneToOneField<User>,
}
```

`OneToOneField<T>` is a thin newtype over `ForeignKey<T>`: same runtime
shape, same `.fetch(&mut ctx)` / `.resolved()` / prefetch surface. The
distinction exists so the macro can:

- emit a singular reverse accessor (see below — one `Option<Profile>`, not a `Vec<Profile>`)
- emit a `UNIQUE` constraint on the FK column in Phase 6 DDL

---

## Reverse Accessors

Foreign keys are one-directional by default: `Vehicle` knows about
`Owner` via `owner_id`, but `Owner` has no method to reach its vehicles.
Two macros opt in to the reverse accessor:

### `reverse_one_to_many!` — many children per parent

```rust
djogi::reverse_one_to_many!(Owner, vehicles -> Vehicle by owner_id);
```

Effective emission:

```rust
// Effective emission — do not write this by hand
impl Owner {
    pub async fn vehicles<'ctx>(
        &'ctx self,
        ctx: &'ctx mut DjogiContext,
    ) -> Result<Vec<Vehicle>, DjogiError>
    {
        Vehicle::objects()
            .filter(|f| f.owner_id().eq(ForeignKey::new(self.id.clone())))
            .fetch_all(ctx)
            .await
    }
}
```

Call site:

```rust
let vehicles: Vec<Vehicle> = owner.vehicles(&mut ctx).await?;
// SELECT * FROM vehicles WHERE owner_id = $1
```

### `reverse_one_to_one!` — at most one child per parent

```rust
djogi::reverse_one_to_one!(User, profile -> Profile by user_id);
```

Effective emission differs in the return type — `Option<T>`, not `Vec<T>` —
and the terminal is `.first(&mut ctx)` instead of `.fetch_all(&mut ctx)`:

```rust
let profile: Option<Profile> = user.profile(&mut ctx).await?;
// SELECT * FROM profiles WHERE user_id = $1 LIMIT 1
```

### Compile-time collision detection

The reverse macros emit **plain inherent methods**, so two reverse macros
that would produce the same accessor name on the same source model fail
at compile time with rustc's standard duplicate-method error:

```rust
djogi::reverse_one_to_many!(Owner, vehicles -> Vehicle by owner_id);
djogi::reverse_one_to_many!(Owner, vehicles -> Truck   by owner_id);
// error[E0592]: duplicate definitions with name `vehicles`
```

A compile-fail fixture pins this: `reverse_relation_duplicate_accessor.rs`.
The `via` column is also validated — an unknown or non-FK column fails
the `const_assert_plain_ident` gate at codegen.

---

## Many-to-Many

Djogi does not generate hidden join tables. A many-to-many relation
always uses an **explicit through model** — one with its own descriptor,
its own table, and its own extra columns (role, joined_at, policy blob).
That makes junction rows first-class queryable via
`Through::objects()`.

### Declaring the models

```rust
use djogi::relation::{ForeignKey, ManyToMany};

#[model(table = "people")]
#[derive(Debug, Clone)]
pub struct Person {
    pub name: String,
}

#[model(table = "groups")]
#[derive(Debug, Clone)]
pub struct Group {
    pub name: String,
}

#[model(table = "person_groups", through, no_default)]
#[derive(Debug, Clone)]
pub struct PersonGroup {
    pub person_id: ForeignKey<Person>,
    pub group_id:  ForeignKey<Group>,
    pub role:      String,
}
```

The `through` flag on `#[model(..., through)]` is load-bearing, not
cosmetic. Through models must declare **at least two `ForeignKey<T>`
fields** or the macro rejects them at compile time
(fixture: `invalid_through_model.rs`). The flag also sets
`ModelDescriptor::is_through = true` so admin/migration tooling can
surface junction models distinctly from application tables.

### Stamping out both directions

```rust
djogi::many_to_many!(
    Person, Group,
    through = PersonGroup,
    this_fk = person_id,
    that_fk = group_id,
    relation = "groups"
);

djogi::many_to_many!(
    Group, Person,
    through = PersonGroup,
    this_fk = group_id,
    that_fk = person_id,
    relation = "members"
);
```

Each invocation emits one direction. The macro generates:

| Item | Shape |
|---|---|
| `impl ManyToMany<Target> for Source` | Supplies `Through`, `RELATION`, `this_fk()`, `that_fk()`, and typed bodies for `related` / `add_related` / `remove_related` |
| `impl Source` inherent method named after `relation` | `person.groups(&mut ctx)` delegates to `<Self as ManyToMany<Group>>::related(self, &mut ctx)` |
| `inventory::submit!(ReverseRelationMarker::new_via_macro_support(...))` | Registers the relation for collision detection and admin enumeration |
| `const _: () = { … }` const-assert block | Validates `relation`, `this_fk`, `that_fk` against the Postgres plain-identifier grammar at codegen time |

All three user-supplied identifiers (`relation`, `this_fk`, `that_fk`)
flow through `const_assert_plain_ident`, so a keyword or SQL-injection
attempt (`"id; DROP TABLE users"`) fails at compile time, not at query
time. Fixtures: `many_to_many_bad_that_fk_keyword.rs` and peers.

### Call sites

```rust
// The `relation = "groups"` argument becomes the accessor method name:
let groups: Vec<Group> = person.groups(&mut ctx).await?;
// SELECT * FROM person_groups WHERE person_id = $1;
// SELECT * FROM groups WHERE id IN ($1, $2, ...);

// Attach:
let group = Group::get(&mut ctx, group_id).await?;
let junction = person.add_related(
    &mut ctx,
    &group,
    PersonGroup {
        role: "admin".into(),
        ..Default::default()
    },
).await?;
// INSERT INTO person_groups (person_id, group_id, role, ...)
// VALUES ($1, $2, $3, ...) RETURNING *;

// Detach:
let removed: u64 = person.remove_related(&mut ctx, &group).await?;
// DELETE FROM person_groups WHERE person_id = $1 AND group_id = $2;
```

`add_related` takes the whole `PersonGroup` by value (not just the extras)
so the caller always keeps control of the junction-specific columns.
The macro overwrites `person_id` / `group_id` with freshly-built
`ForeignKey` values, then persists the row — `role`, `joined_at`, and
any other non-FK columns survive untouched.

### Querying the junction directly

The through model stays queryable. Reach for `Through::objects()` when
you need to filter or join on junction-specific data:

```rust
let admins: Vec<PersonGroup> = PersonGroup::objects()
    .filter(|f| f.role().eq("admin".to_string()))
    .fetch_all(&mut ctx)
    .await?;
// SELECT * FROM person_groups WHERE role = $1
```

### Accessor-name collisions

Two `many_to_many!` invocations on the same source that produce the same
`relation` name fail with rustc's duplicate-method error — same mechanism
as the reverse macros. The inventory marker is informational for admin
tooling, not the collision gate.

### Roadmap: view struct

A future iteration will optionally emit a `{Source}{Relation}View` struct
that combines through-row and target-row fields into one flattened shape
— useful when callers want both sides of the junction without two
round trips. It depends on the `__DJOGI_FIELD_NAMES` descriptor
infrastructure extension and lands after Phase 4's expression layer. In
Phase 3 the two-query `Vec<Target>` shape above is the supported path.

---

## Descriptor Integration

Every relation registers a descriptor entry alongside the model
descriptor. Iterate them with:

```rust
for desc in inventory::iter::<djogi::ModelDescriptor> {
    for rel in desc.relations {
        println!("{}.{} -> {} ({:?})", desc.type_name, rel.name, rel.target, rel.kind);
    }
}
```

`RelationKind` has variants `FK`, `O2O`, `ReverseFK`, `ReverseO2O`,
`M2M` — the migration differ, admin panel, and shell use this to render
the relation graph without parsing struct sources.

---

## See Also

- [Models](./models.md) — model attributes, field attributes, descriptor basics
- [Queries](./queries.md) — `QuerySet`, filters, `FieldRef` accessors
- [Relations roadmap](../roadmap/relations.md) — multi-hop prefetch, generic FKs, typed `IN` on FK sets
- [Relations spec](../spec/relations.md) — design rationale, invariants, rejected alternatives
