> [Back to README](../../ReadMe.MD) | [All Guides](./index.md)

# Relations

Phase 3 ships Djogi's typed relation layer: `ForeignKey<T>`,
`OneToOneField<T>`, explicit eager loading through `prefetch(...)` and
`select_related(...)`, reverse accessor macros, and explicit-through
many-to-many helpers.

Djogi keeps relations explicit:

- no lazy loading
- no string paths
- no implicit many-to-many tables

If a relation touches the database, the call site says so.

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

`ForeignKey<T>` stores the target model's primary key, not an eagerly loaded
row. Fetching the related row is explicit:

```rust
let owner = vehicle.owner_id.fetch(&pool).await?;
```

Use `Option<ForeignKey<T>>` for nullable FK columns.

### Eager Loading

Use the macro-emitted `{Model}Related` bag with `prefetch(...)` or
`select_related(...)`:

```rust
let vehicles = Vehicle::objects()
    .prefetch(VehicleRelated::owner())
    .fetch_all(&pool)
    .await?;

let joined = Vehicle::objects()
    .select_related(VehicleRelated::owner())
    .fetch_all(&pool)
    .await?;
```

Choose based on query shape:

- `prefetch(...)`: separate query per relation, avoids row explosion
- `select_related(...)`: single `LEFT JOIN`, useful for singular relations

No call to `.owner()` or field access triggers an implicit query.

---

## One-to-One

Use `OneToOneField<T>` when the schema guarantees uniqueness on the foreign
side:

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

`OneToOneField<T>` uses the same explicit fetch / prefetch /
`select_related(...)` pattern as `ForeignKey<T>`.

---

## Reverse Accessors

Phase 3 ships two reverse accessor macros:

```rust
djogi::reverse_one_to_many!(Owner, vehicles -> Vehicle by owner_id);
djogi::reverse_one_to_one!(User, profile -> Profile by user_id);
```

They emit inherent methods on the source model:

```rust
let vehicles = owner.vehicles(&pool).await?;
let profile = user.profile(&pool).await?;
```

These are plain generated methods, so duplicate accessor names on the same
receiver fail at compile time.

---

## Many-to-Many

Djogi does not generate hidden join tables. A many-to-many relation always
uses an explicit through model:

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
    pub group_id: ForeignKey<Group>,
    pub role: String,
}

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

This yields:

- `impl ManyToMany<Group> for Person`
- `impl ManyToMany<Person> for Group`
- `person.groups(&pool).await?`
- `group.members(&pool).await?`

Mutation helpers are explicit too:

```rust
let group = Group::get(&pool, group_id).await?;

person
    .add_related(
        &pool,
        &group,
        PersonGroup {
            role: "admin".into(),
            ..Default::default()
        },
    )
    .await?;

person.remove_related(&pool, &group).await?;
```

The through model remains a normal `Model`, so you can query relation-specific
columns directly:

```rust
let admins = PersonGroup::objects()
    .filter(|f| f.role.eq("admin"))
    .fetch_all(&pool)
    .await?;
```

`#[model(..., through)]` is not just metadata: through models must declare at
least two `ForeignKey<T>` fields or the macro rejects them at compile time.

---

## See Also

- [Models](./models.md)
- [Queries](./queries.md)
- [Relations spec](../spec/relations.md)
