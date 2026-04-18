> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# Protected Data Metadata & Field Codecs

Djogi needs a way to describe sensitive fields mechanically and to attach storage/serialization rules that all generated surfaces can respect.

This spec defines the descriptor-level primitives for protected data. It does **not** define full governance execution or legal workflow behavior.

---

## Goals

- mark fields as sensitive in model metadata
- centralize protection semantics in one place
- support field codecs for transformed storage
- let later phases reuse the same metadata for projections, admin, logs, exports, and lifecycle tooling

---

## Minimal Public Surface

Phase 6.5 should stabilize only the descriptor-facing primitives:

Field annotations:

```rust
#[field(sensitive)]
#[field(rationale = "contains personal contact data")]
#[field(redact_in(public, logs))]
#[field(codec = "encrypted")]
#[field(retention_class = "anonymize")]
```

Descriptor expectations:

- a field can declare whether it is sensitive
- a field can declare where it must be redacted
- a field can declare a codec identifier
- a field can carry rationale text
- a field can declare a lifecycle class label

Runtime expectations:

- CRUD writes apply the codec on persistence
- row loading applies the codec on decode
- generated projections/admin defaults can inspect the same metadata

The first shipping version does not need a large codec ecosystem. It only needs one stable contract for how codecs are declared and discovered.

---

## Protected Field Metadata

Djogi supports field-level protection annotations such as:

- `sensitive`
- `rationale = "..."`
- `redact_in(...)`
- `expose(...)`
- `retention_class = "..."`

Not every annotation needs to become active behavior immediately, but the descriptor must be able to carry them.

These annotations are not application policy by themselves. They are metadata that later framework surfaces can interpret consistently.

---

## Field Codecs

Some fields require a storage transform between the Rust type and the database representation.

Examples of supported codec categories:

- encrypted-at-rest columns
- tokenized fields
- custom serialized payloads
- format-preserving wrappers

The codec contract belongs in Djogi because:

- CRUD generation must encode values consistently
- row decoding must reverse the transform consistently
- admin/shell/logging/export behavior must know that the field is protected

Field codecs are a data-layer concern, not an HTTP or UI concern.

---

## Descriptor Contract

`FieldDescriptor` should eventually carry enough metadata to answer:

- is this field sensitive?
- in which projections or surfaces may it appear?
- does it use a storage codec?
- what rationale accompanies the protection choice?
- what retention/lifecycle class is associated with it?

The runtime does not need to execute every one of those dimensions in Phase 6.5, but the descriptor shape must be able to express them.

---

## Logging & Admin Interaction

Protected-field metadata must influence later framework surfaces:

- admin forms should not accidentally expose protected fields
- shell output should have safe defaults
- audit logs should support redaction-aware diffs
- generated projections should fail if they include disallowed fields

This is why protected-data metadata belongs in Djogi core rather than in handwritten application DTOs.

---

## Non-Goals

Phase 6.5 does not include:

- legal-hold workflow
- purge/anonymize/archive execution
- export bundle delivery
- KMS integration policy
- application-specific authorization

Those belong to later governance phases or companion crates.
