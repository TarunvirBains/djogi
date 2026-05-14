> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# JSONB Schema Fields

## 6. JSONB Schema Fields — `Jsonb<T>`

`Jsonb<T>` is a first-class Djogi field type that combines Postgres's `JSONB` column with a Rust schema type, serde deserialization, and validator-based validation. Unlike bare JSON fields that store and retrieve without any schema awareness, `Jsonb<T>` enforces typed schemas while preserving unknown fields.
### 6.1 Defining a JSONB Field
```rust
use djogi::prelude::*;

#[derive(JsonSchema, Serialize, Deserialize, Validate)]
pub struct TurboSpec {
    pub boost_psi: f64,
    #[validate(length(min = 1))]
    pub manufacturer: String,
}

#[derive(JsonSchema, Serialize, Deserialize, Validate)]
pub struct EngineSpec {
    pub cylinders: i32,
    #[validate(range(min = 0, max = 2000))]
    pub horsepower: i32,
    pub turbo: Option<Jsonb<TurboSpec>>,   // nested schema — fully typed
}

#[model(table = "vehicles")]
pub struct Vehicle {
    pub make: String,
    pub engine: Jsonb<EngineSpec>,         // JSONB column in Postgres
}
```
`Jsonb<T>` stores a typed value plus preserved unknown keys. Database encode/decode uses serde; typed path querying requires `T: JsonbSchema` (normally via `#[derive(JsonbSchema)]`).
Nested `Jsonb<T>` is fully supported — each level of nesting has its own typed schema with its own known/unknown field boundary. There is no depth limit.
### 6.2 Internal Layout
```rust
pub struct Jsonb<T> {
    pub data: T,                            // fully typed value
    extra: IndexMap<String, UnknownField>,  // unknown fields — preserved, never dropped
}
```
### 6.3 Unknown Field Preservation

Fields present in the stored JSON but absent from the schema are never dropped on save. They are loaded into `extra`, accessible at runtime, and round-tripped untouched through every `save()`.

Given this JSON in the database:
```json
{
  "cylinders": 8,
  "horsepower": 450,
  "turbo": {
    "boost_psi": 18.5,
    "manufacturer": "Garrett",
    "part_number": "GT3582R"
  },
  "legacy_ecu_code": "M62B44"
}
```
- `cylinders`, `horsepower`, `turbo` — known fields, fully typed via `EngineSpec`
- `turbo.part_number` — unknown at `TurboSpec` level, preserved in `TurboSpec`'s `extra`
- `legacy_ecu_code` — unknown at `EngineSpec` level, preserved in `EngineSpec`'s `extra`

On save, all unknown fields are written back exactly as loaded. No data is ever silently destroyed.

### 6.4 The `UnknownField` Type

Unknown fields surface as `serde_json::Value` through the `UnknownField` type alias. Conversion helpers live on `UnknownFieldExt`; nested objects and arrays remain JSON values rather than becoming a second dynamic schema system.
```rust
pub type UnknownField = serde_json::Value;
```
### 6.5 Accessing Unknown Fields — `UnknownFieldError`

All conversions return `Result` — never `Option` or a raw panicking value. Implicit coercion between types is never performed. A string that looks like a number is not silently converted.
```rust
pub enum UnknownFieldError {
    // Field key does not exist at this schema level
    FieldNotFound { field: String },
    // Asked for f64, it is stored as String
    TypeMismatch { field: String, expected: &'static str, actual: &'static str },
    // String value that looks like the requested type — coercion refused
    NoImplicitCoercion { field: String, value: String, into: &'static str },
}
```
Examples:
```rust
// DB has: "legacy_ecu_code": "M62B44"
car.engine.extra().get("legacy_ecu_code").unwrap().try_as_str()
// Ok("M62B44")

car.engine.extra().get("legacy_ecu_code").unwrap().try_as_f64()
// Err(TypeMismatch { field: "legacy_ecu_code", expected: "f64", actual: "String" })

// DB has: "boost_psi": "18.5"  ← string, not float — data quality problem
turbo.extra().get("boost_psi").unwrap().try_as_f64()
// Err(NoImplicitCoercion { field: "boost_psi", value: "18.5", into: "f64" })
// Not silently Ok(18.5) — the caller must fix the data

// Field does not exist at this level
car.engine.extra().get("nonexistent")
// Err(FieldNotFound { field: "nonexistent" })

// Nested unknown traversal
car.engine.data.turbo
    .as_ref()
    .and_then(|t| t.extra().get("part_number"))
    .and_then(|f| f.as_str().ok())
// Some("GT3582R")

// Inspect all unknown fields at a level
for (key, val) in car.engine.extra() {
    println!("{}: {:?}", key, val);
}
```
### 6.6 Validation on Save

Validation runs through the full schema tree before any write touches the database. If any level fails, the save is aborted with a structured error — nothing is written.
```rust
car.engine.data.horsepower = 5000;  // exceeds range(max = 2000)
car.save(&mut ctx).await?;
// Err: validation failed: engine.horsepower must be <= 2000

car.engine.data.turbo = Some(Jsonb::new(TurboSpec {
    boost_psi: 18.5,
    manufacturer: "".into(),   // violates length(min = 1)
}));
car.save(&mut ctx).await?;
// Err: validation failed: engine.turbo.manufacturer must not be empty
```
Error paths use dot-notation through the full nesting depth so the developer knows exactly where the failure is.
### 6.7 Subfield Query Filters

The proc macro generates typed filter accessors for all known fields at every nesting level, using Postgres's JSONB path operators. Accessors are method-style — `.engine().horsepower()` rather than `.engine.horsepower` — so the derive can honour `#[serde(rename)]` and visibility settings on the schema struct. Enter the typed path tree via `.typed()` on the outer `JsonbPathRef` returned by the model's `Fields` accessor.
```rust
// Known field at root level
Vehicle::objects()
    .filter(|f| f.engine().typed().horsepower().gte(300))
    // WHERE (engine->>'horsepower')::integer >= 300

// Known field in a nested plain-struct schema (EngineSpec has a `turbo: TurboSpec`)
Vehicle::objects()
    .filter(|f| f.engine().typed().turbo().boost_psi().gte(15.0))
    // WHERE (engine->'turbo'->>'boost_psi')::float >= 15.0

Vehicle::objects()
    .filter(|f| f.engine().typed().turbo().manufacturer().eq("Garrett"))
    // WHERE engine->'turbo'->>'manufacturer' = 'Garrett'
```
Unknown fields cannot be used in typed filter closures because they are not known at compile time. Query them through an explicit raw-SQL helper guarded by `#[djogi::deliberately_bypass_convention_with_raw_sql]` and a local `JUSTIFICATION` comment.
### 6.8 Shell Access
```rhai
let car = Vehicle::get(42);

// Known fields — typed, direct
print(car.engine.horsepower);
print(car.engine.turbo.boost_psi);

// Unknown fields — Result-returning, explicit
print(car.engine.extra().get("legacy_ecu_code").unwrap().try_as_str());
print(car.engine.turbo.extra().get("part_number").unwrap().try_as_str());

// Inspect all unknowns
pp(car.engine.extra());

// Filter by nested known field
let powerful = Vehicle::objects()
    .filter_struct(VehicleFilter::new().engine_horsepower(Gte(300)))
    .fetch_all();
```
