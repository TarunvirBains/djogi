> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

## 8. Relations

### 8.1 ForeignKey
```rust
#[derive(Model)]
#[model(table = "vehicles")]
pub struct Vehicle {
    pub make: String,
    #[field(on_delete = "cascade")]
    pub owner_id: ForeignKey<Owner>,
    pub fuel_type_id: ForeignKey<FuelType>,   // defaults to RESTRICT
}
```
Eager loading — explicit, no lazy loading:
```rust
// Single fetch — one query, explicit
let owner = car.owner_id.fetch(&pool).await?;

// Prefetch on QuerySet — one IN (...) query per relation, not N+1
let cars = Vehicle::objects()
    .prefetch(VehicleRelated::owner())
    .prefetch(VehicleRelated::fuel_type())
    .fetch_all(&pool).await?;

let owner = cars[0].owner_id.resolved();   // -> Option<&Owner>, free after prefetch
```
No lazy loading. No surprise queries. The developer always knows when the DB is hit.
### 8.2 Many-to-Many — Explicit Through Models

Implicit M2M fields are not provided. All M2M relationships require an explicit through model — this avoids the forced migration that implicit M2M fields eventually require when you need to store data on the relationship.
```rust
#[derive(Model)]
pub struct Person {
    pub name: String,
}

#[derive(Model)]
pub struct Group {
    pub name: String,
}

#[derive(Model)]
#[model(table = "person_groups", through)]
pub struct PersonGroup {
    pub person_id: ForeignKey<Person>,
    pub group_id: ForeignKey<Group>,
    pub joined_at: DateTime,
    pub role: String,
}
```
Declaring the relationship — both directions are explicit:
```rust
impl ManyToMany<Group> for Person {
    type Through = PersonGroup;
    const RELATION: &'static str = "groups";    // generates person.groups()
}

impl ManyToMany<Person> for Group {
    type Through = PersonGroup;
    const RELATION: &'static str = "members";   // generates group.members()
}
```
Explicit naming via `RELATION` is intentional — auto-pluralization is error-prone and the developer's domain language is more expressive (`members` vs `persons`).
Generated convenience methods:
```rust
// Person side
let groups = person.groups(&pool).await?;
person.add_to_group(&pool, &group, PersonGroup {
    role: "admin".into(), ..Default::default()
}).await?;
person.remove_from_group(&pool, &group).await?;

// Group side
let members = group.members(&pool).await?;
group.add_to_member(&pool, &person, PersonGroup { ... }).await?;
group.remove_from_member(&pool, &person).await?;

// Through model is a full Model — directly queryable
let admins = PersonGroup::objects()
    .filter(|f| f.role.eq("admin"))
    .fetch_all(&pool).await?;
```
---
