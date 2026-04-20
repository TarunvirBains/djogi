> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# Projections & Shared Contracts

Djogi models are backend truths. Frontends, APIs, admin surfaces, and export flows should usually consume derived projection types rather than raw model structs.

This spec defines projection generation as a first-class framework capability.

---

## Goals

Projection generation exists to solve four recurring problems:

1. Prevent raw persistence models from leaking across transport boundaries.
2. Generate audience-specific views from one model definition.
3. Keep field-visibility rules centralized in model metadata instead of duplicating mapping code.
4. Produce Rust types that can be shared safely between backend and frontend crates.

Projection generation is backend-first but not server-only. Djogi does not own frontend rendering, but it may generate transport-safe and UI-safe Rust types from model descriptors.

---

## Minimal Public Surface

Phase 4.5 should stabilize only this much public surface:

Model-side annotations:

```rust
#[model(table = "users")]
pub struct User {
    #[field(expose(public, self_view, admin, export))]
    pub display_name: String,

    #[field(expose(self_view, admin, export))]
    pub email: String,

    #[field(expose(none))]
    pub password_hash: String,
}
```

Generated types:

```rust
UserPublic
UserSelfView
UserAdmin
UserExport
```

Generated conversions split on whether the projection nests a peer
projection through a relation field:

```rust
// Scalar-only projection — infallible.
impl From<&User> for UserPublic

// Relation-nesting projection (at least one `expose(scope = "Peer")`
// entry on a `ForeignKey<T>` / `OneToOneField<T>` field). Returns
// `ProjectionError::UnresolvedRelation { model, field, scope }` when
// the relation wasn't prefetched / selected before the conversion.
impl TryFrom<&Vehicle> for VehiclePublic {
    type Error = djogi::ProjectionError;
    // ...
}
```

`ProjectionError` is `#[non_exhaustive]` so later phases (protected-data,
codec failures) can add variants without a breaking change. Callers
matching on the error must include `_ => ...`.

Generated types must:

- be plain Rust structs
- derive `Debug`, `Clone`, `Serialize`, `Deserialize` unconditionally
- avoid SQLx/runtime traits
- be importable by shared API/frontend crates

`internal` is accepted as a grammar sentinel equivalent to `none` — no
`{Model}Internal` struct is generated. The model struct itself IS the
internal form.

`Option<ForeignKey<T>>` / `Option<OneToOneField<T>>` in relation-form
`expose` is rejected with a loud compile error in Phase 4.5 — cross-model
dispatch of `Option<&T>` → peer projection is a follow-up-phase
extension.

Anything beyond that is additive and should not block the first spec closure.

---

## Core Rule

The persistence model is not the public contract.

Djogi treats a `#[model]` struct as:

- the schema definition
- the query/runtime type
- the source of truth for derivable projections

It does **not** assume that the same struct should be serialized directly to clients.

---

## Projection Scopes

Djogi supports named projection scopes. The built-in canonical scopes are:

- `public`
- `self_view`
- `admin`
- `export`
- `internal`

Applications may define additional named scopes later, but these built-ins cover the common transport boundaries.

Each field may opt into one or more scopes.

Example:

```rust
#[model(table = "users")]
pub struct User {
    #[field(expose(public, self_view, admin, export))]
    pub display_name: String,

    #[field(expose(self_view, admin, export))]
    pub email: String,

    #[field(expose(none))]
    pub password_hash: String,

    #[field(expose(admin))]
    pub internal_notes: Option<String>,
}
```

This metadata is compile-time descriptor data, not a runtime permission system.

---

## Generated Types

For each requested projection scope, Djogi generates a concrete Rust struct.

Example:

```rust
pub struct UserPublic {
    pub display_name: String,
}

pub struct UserSelfView {
    pub display_name: String,
    pub email: String,
}

pub struct UserAdmin {
    pub display_name: String,
    pub email: String,
    pub internal_notes: Option<String>,
}
```

Generated projection types:

- derive `Serialize` / `Deserialize` when their fields support it
- are independent of SQLx and database connection traits
- are intended to be imported by API and frontend crates

Djogi does not generate UI components, hooks, routes, or frontend state containers.

---

## Projection Conversions

Djogi generates conversions from model to projection.

Required baseline:

- `impl From<&Model> for Projection`

Optional additive support later:

- owned conversion variants
- fallible conversions when projection rules require transformation

The point is to replace handwritten mapping layers that are repetitive and prone to drift.

---

## Relations

Projections may include related data, but only through projected forms.

Rules:

- a projection must never include a raw related persistence model
- related fields included in a projection must point to a named projection for the related model
- relation loading semantics remain explicit; projection generation does not imply lazy loading

Relation fields reuse the same `expose(...)` attribute as scalars, with a key-value form that names the nested projection per scope:

```rust
#[field(expose(public = "UserSummary", self_view = "UserDetail", admin))]
pub owner: ForeignKey<User>,
```

Form semantics:

- `expose(scope)` on a scalar field — include as the native type in `scope`.
- `expose(scope = "ProjType")` on a relation field — include in `scope` rendered as the named nested projection. The macro rejects this form on scalar fields.
- `expose(scope)` on a relation field — reserved; the macro should require an explicit nested projection name rather than fall back to the raw persistence model.

The exact syntax may evolve, but the contract is stable: nested transport shapes must remain projection-based, and the attribute name stays `expose` so scope membership lives in one place.

---

## Typed JSON Fields

When a model contains typed JSON-backed fields, projection rules apply at the field boundary first.

Baseline behavior:

- include the whole typed JSON field
- exclude the whole typed JSON field

Later additive behavior may allow subfield projection, but Phase 4.5 only requires field-level control.

---

## Compile-Time Validation

Projection generation must fail at compile time when:

- a projection references a field excluded from that scope
- two generated projection names collide
- a nested projection references a missing related projection
- a projection requests a field whose type is not serializable for its requested derive set

Djogi should prefer compile-time diagnostics over runtime surprises.

---

## Deferred Surface

The following are explicitly deferred beyond the minimum Phase 4.5 surface:

- custom user-defined projection scopes
- projection renaming rules beyond the default canonical names
- partial JSON subfield projections
- fallible transforms during projection generation
- route-specific wrapper DTO generation

The first shipping surface should stay small.

---

## Descriptor Integration

Projection metadata belongs in `ModelDescriptor` / `FieldDescriptor`.

This is important because the same exposure rules should later inform:

- admin generation
- export generation
- redaction behavior
- shell display defaults
- protected-field governance

Projection support is therefore not just DTO codegen. It is a foundational contract layer for later phases.

---

## Shared-Crate Use

Generated projections are intended to support a shared-contract crate pattern:

- backend route handlers return projection structs
- frontend crates import the same projection structs
- raw models stay in backend/data crates

This keeps persistence concerns and transport concerns separate without duplicating type definitions by hand.

---

## Explicit Non-Goals

This spec does not include:

- frontend framework integration
- route generation
- authorization decisions
- runtime policy enforcement
- retention/governance behavior
- export packaging or ZIP generation

Those are either higher-level application concerns or later Djogi phases.
