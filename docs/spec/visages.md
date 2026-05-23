> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# Visages & Shared Contracts

Djogi models are backend truths. Frontends, APIs, admin surfaces, and export flows should usually consume derived visage types rather than raw model structs.

This spec defines visage generation as a first-class framework capability.

---

## Goals

Visage generation exists to solve four recurring problems:

1. Prevent raw persistence models from leaking across transport boundaries.
2. Generate audience-specific views from one model definition.
3. Keep field-visibility rules centralized in model metadata instead of duplicating mapping code.
4. Produce Rust types that can be shared safely between backend and frontend crates.

Visage generation is backend-first but not server-only. Djogi does not own frontend rendering, but it may generate transport-safe and UI-safe Rust types from model descriptors.

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

Generated conversions split on whether the visage nests a peer
visage through a relation field:

```rust
// Scalar-only visage — infallible.
impl From<&User> for UserPublic

// Relation-nesting visage (at least one `expose(scope = "Peer")`
// entry on a `ForeignKey<T>` / `OneToOneField<T>` field). Returns
// `VisageError::UnresolvedRelation { model, field, scope }` when
// the relation wasn't prefetched / selected before the conversion.
impl TryFrom<&Vehicle> for VehiclePublic {
    type Error = djogi::VisageError;
    // ...
}
```

`VisageError` is `#[non_exhaustive]` so later phases (protected-data,
codec failures) can add variants without a breaking change. Callers
matching on the error must include `_ => ...`.

Generated types must:

- be plain Rust structs
- derive `Debug`, `Clone`, `Serialize`, `Deserialize` unconditionally
- avoid `tokio-postgres` / runtime traits
- be importable by shared API/frontend crates

`internal` is accepted as a grammar sentinel equivalent to `none` — no
`{Model}Internal` struct is generated. The model struct itself IS the
internal form.

Phase 7-Zero-2 lifted the Phase 4.5 deferral on optional FK / O2O
visages: `Option<ForeignKey<T>>` / `Option<OneToOneField<T>>` now
project as `Option<PeerVisage>` and participate in traversal under
the `->` grammar. Phase 4.5's compile-rejection no longer applies.

Anything beyond that is additive and should not block the first spec closure.

---

## Core Rule

The persistence model is not the public contract.

Djogi treats a `#[model]` struct as:

- the schema definition
- the query/runtime type
- the source of truth for derivable visages

It does **not** assume that the same struct should be serialized directly to clients.

---

## Visage Scopes

Djogi supports named visage scopes. The built-in canonical scopes are:

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

For each requested visage scope, Djogi generates a concrete Rust struct.

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

Generated visage types:

- derive `Serialize` / `Deserialize` when their fields support it
- are independent of `tokio-postgres` and database connection traits
- are intended to be imported by API and frontend crates

Djogi does not generate UI components, hooks, routes, or frontend state containers.

---

## Visage Conversions

Djogi generates conversions from model to visage. The macro
dispatches on whether the visage nests a peer visage through
a relation field:

- Scalar-only visage (no `expose(scope = "Peer")` entries) —
  `impl From<&Model> for Visage`. Infallible; straight-line
  construction.
- Relation-nesting visage (at least one `expose(scope = "Peer")`
  on a `ForeignKey<T>` / `OneToOneField<T>` field) —
  `impl TryFrom<&Model> for Visage` with
  `type Error = djogi::VisageError`. Returns
  `VisageError::UnresolvedRelation { model, field, scope }` when
  the relation wasn't prefetched / selected before the conversion.

Scalar-only visages also satisfy `TryFrom<&Model>` via the stdlib
blanket `impl<T, U> TryFrom<U> for T where U: Into<T>` with
`Error = Infallible`, and `impl From<Infallible> for VisageError`
bridges the two error types so nested `try_from(..)?` calls compose
uniformly. That is what lets a relation-nesting visage embed a
scalar-only peer without the emitter knowing the peer's shape.

Optional additive support later:

- owned conversion variants
- user-defined fallible transforms beyond `UnresolvedRelation`

The point is to replace handwritten mapping layers that are repetitive and prone to drift.

---

## Custom Scope Declaration

### Syntax: `visage_scopes`

Applications can define custom visage scopes beyond the built-in canonical scopes (`public`,
`self_view`, `admin`, `export`, `internal`). Declare them with the `visage_scopes` argument
on `#[model(...)]`:

```rust
#[model(
    table = "users",
    visage_scopes(support = Support)
)]
pub struct User {
    #[field(expose(public, support))]
    pub email: String,
}
```

### Generated Visage Naming

The macro generates a visage struct for each custom scope using the pattern `{Model}{Suffix}`:

```
visage_scopes(support = Support)  // on User → generates UserSupport
```

### Infallibility

A custom scope visage is infallible (`From<&Model>`) when no field in that scope uses
`try_presentation_codec`. If any field in the scope uses `try_presentation_codec`, the
generated visage implements `TryFrom<&Model>` instead.

### Framework Fields

Generated custom scope visages always include `id`, `created_at`, and `updated_at`, with
values matching the source model.

### Scope Coverage

Custom scope names declared with `visage_scopes` are valid identifiers inside `expose(...)`
field annotations on the same model. A field with `expose(public, support)` appears in both
the built-in `UserPublic` visage and the custom `UserSupport` visage.

For built-in scope names and their generated visage types, see [Visage Scopes](#visage-scopes)
above.

---

## Relations

Visages may include related data, but only through projected forms.

Rules:

- a visage must never include a raw related persistence model
- related fields included in a visage must point to a named visage for the related model
- relation loading semantics remain explicit; visage generation does not imply lazy loading

Relation fields reuse the same `expose(...)` attribute as scalars under
the Phase 7-Zero-2 `->` grammar:

```rust
// Narrow peer visage per scope.
#[field(expose(public -> UserSummary, self_view -> UserDetail))]
pub owner: ForeignKey<User>,

// ID-only on `public` (no `->`); narrow peer visage on `admin`.
#[field(expose(public, admin -> UserAdmin))]
pub owner: ForeignKey<User>,

// Full-struct embedding — emit the full model where you need it.
#[field(expose(self_view -> User))]
pub owner: ForeignKey<User>,
```

Form semantics:

- `expose(scope)` on a scalar field — include as the native type in `scope`.
- `expose(scope)` on a relation field — include the FK column in `scope`
  as an ID-only projection.
- `expose(scope -> Peer)` on a relation field — include in `scope`
  rendered as the named peer visage (or as the model struct itself
  when `Peer` names the model). The macro rejects the `->` form on
  scalar fields.
- `Option<ForeignKey<T>>` / `Option<OneToOneField<T>>` follow the same
  grammar and project as `Option<PeerVisage>`.

The contract is stable: nested transport shapes must remain visage- or
model-based, and the attribute name stays `expose` so scope membership
lives in one place.

---

## Typed JSON Fields

When a model contains typed JSON-backed fields, visage rules apply at the field boundary first.

Baseline behavior:

- include the whole typed JSON field
- exclude the whole typed JSON field

Later additive behavior may allow subfield visage, but Phase 4.5 only requires field-level control.

---

## Compile-Time Validation

Visage generation must fail at compile time when:

- a visage references a field excluded from that scope
- two generated visage names collide
- a nested visage references a missing related visage
- a visage requests a field whose type is not serializable for its requested derive set

Djogi should prefer compile-time diagnostics over runtime surprises.

---

## Deferred Surface

The following are explicitly deferred beyond the minimum Phase 4.5 surface:

- visage renaming rules beyond the default canonical names — when this
  feature is taken up, the spec that introduces it MUST address the
  relation-embedding declaration-site churn flagged in
  [`docs/research/model-vs-visage-lower-severity-graduation.md`](../research/model-vs-visage-lower-severity-graduation.md)
  §Surface 3 (rename of `User::Public` → `User::Summary` cannot force
  touching every source model that embeds the visage; a target-side
  `#[model(embeddable_as = [...])]` alias or equivalent indirection
  is one candidate shape)
- partial JSON subfield visages
- fallible transforms during visage generation
- route-specific wrapper DTO generation

The first shipping surface should stay small.

---

## Descriptor Integration

Visage metadata belongs in `ModelDescriptor` / `FieldDescriptor`.

This is important because the same exposure rules should later inform:

- admin generation
- export generation
- redaction behavior
- shell display defaults
- protected-field governance

Visage support is therefore not just DTO codegen. It is a foundational contract layer for later phases.

---

## Shared-Crate Use

Generated visages are intended to support a shared-contract crate pattern:

- backend route handlers return visage structs
- frontend crates import the same visage structs
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
