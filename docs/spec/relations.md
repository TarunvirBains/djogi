> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

## 8. Relations

### 8.1 ForeignKey
```rust
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
let owner = car.owner_id.fetch(&mut ctx).await?;

// Prefetch on QuerySet — one IN (...) query per relation, not N+1
let cars = Vehicle::objects()
    .prefetch(VehicleRelated::owner())
    .prefetch(VehicleRelated::fuel_type())
    .fetch_all_prefetched(&mut ctx).await?;

let owner = cars[0].owner_id.resolved();   // -> Option<&Owner>, free after prefetch
```
No lazy loading. No surprise queries. The developer always knows when the DB is hit.

Transport nesting: when a relation is surfaced through an [audience projection](./projections.md), it must point to a named projection of the related model (e.g. `#[field(expose(public = "UserSummary"))]` on `owner_id`), not at the raw persistence struct. Projection nesting and relation prefetch are independent — prefetch decides when the relation is loaded; the projection decides what shape it takes at a transport boundary.

### 8.2 Many-to-Many — Explicit Through Models

Implicit M2M fields are not provided. All M2M relationships require an explicit through model — this avoids the forced migration that implicit M2M fields eventually require when you need to store data on the relationship.
```rust
#[model(table = "people")]
pub struct Person {
    pub name: String,
}

#[model(table = "groups")]
pub struct Group {
    pub name: String,
}

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
let groups = person.groups(&mut ctx).await?;
person.add_to_group(&mut ctx, &group, PersonGroup {
    role: "admin".into(), ..Default::default()
}).await?;
person.remove_from_group(&mut ctx, &group).await?;

// Group side
let members = group.members(&mut ctx).await?;
group.add_to_member(&mut ctx, &person, PersonGroup { ... }).await?;
group.remove_from_member(&mut ctx, &person).await?;

// Through model is a full Model — directly queryable
let admins = PersonGroup::objects()
    .filter(|f| f.role.eq("admin"))
    .fetch_all(&mut ctx).await?;
```
---
