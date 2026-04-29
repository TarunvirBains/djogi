> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# Protected Data Metadata & Field Codecs

Djogi needs a way to describe sensitive fields mechanically and to attach storage/serialization rules that all generated surfaces can respect.

This spec defines the descriptor-level primitives for protected data. It does **not** define full governance execution or legal workflow behavior.

> **Status:** Descriptor-side primitives shipped in Phase 7.5. Runtime
> activation (codec encode/decode on CRUD, lifecycle execution,
> redaction-aware audit logs) is staged across Phase 8 and the
> governance/observability phases that follow.

---

## Goals

- mark fields as sensitive in model metadata
- centralize protection semantics in one place
- support field codecs for transformed storage
- let later phases reuse the same metadata for visages, admin, logs, exports, and lifecycle tooling

---

## Minimal Public Surface

Phase 7.5 stabilizes the descriptor-facing primitives via a single
grouped attribute. Five named keys live inside `protected(...)`:

```rust
#[field(protected(
    sensitivity = "pii",
    rationale   = "contains personal contact data",
    redaction   = "hash_id",
    codec       = "encrypted",
    retention   = "anonymize",
))]
pub email: String,
```

All five keys are optional. Adopters typically declare only the
applicable subset — a field that just needs an audit-log redaction
rule writes `protected(redaction = "mask")` with no sensitivity, codec,
or retention. The grammar is order-insensitive and rejects duplicate
keys with a span-precise error.

For ergonomics the rationale string is also accepted as a flat
attribute outside `protected(...)`:

```rust
#[field(rationale = "captured at signup; read by export jobs only")]
```

The two forms are equivalent — the macro lowers both into the same
`ProtectedFieldMetadata::rationale` slot. Flat `rationale` is the
quality-of-life affordance for fields that carry no other protection
metadata; the grouped form is canonical when any other key is set.

### Field annotations recognised

- `protected(sensitivity = "...")` — five-level enum:
  `none`, `internal`, `pii`, `sensitive`, `secret`. The default is
  `none`; declaring it explicitly is allowed but redundant.
- `protected(rationale = "...")` (or flat `rationale = "..."`) — free
  text. Required when sensitivity is above `none` so audit
  surfaces always carry the "why" alongside the classification.
- `protected(redaction = "...")` — named redaction policy. Phase 7.5
  ships `none`, `mask`, `hash_id`, `truncate`, plus an `enclave` slot
  reserved for sensitive enclaves the runtime activates in later
  phases. `hash_id` is constrained to PK-shaped types
  (HeerId / RanjId / Serial); the macro hard-errors when it sees
  `redaction = "hash_id"` on a string or numeric column.
- `protected(codec = "...")` — codec identifier. Resolved against the
  compile-time codec registry (`djogi::field_codec`). Adopters declare
  custom codecs via `#[djogi::field_codec]`-marked types; the
  macro rejects unknown codec strings at expansion time.
- `protected(retention = "...")` — retention/lifecycle label. Phase
  7.5 ships `standard`, `transient`, `legal_hold`, `anonymize`. The
  set is closed; future labels go through a spec amendment.

### Visage-scope axis

Where a field appears (which visage scopes it's projected into) is a
separate axis from how it's redacted within a scope. Membership is
declared with `expose(...)`:

```rust
#[field(expose(self_view, admin, export))]
pub phone: String;
```

`expose(public)` makes a field visible in the public visage at all;
omitting `public` from the list omits the field entirely from that
scope. Within a visage that DOES include the field,
`protected(redaction = "...")` decides how the value is rendered when
a redaction-aware logger or audit surface emits it — the two axes
compose naturally: `expose` answers "is this field present at all?",
`protected` answers "if present in a redaction-aware context, how is
it shown?".

There is intentionally no third axis for "include-but-redact in scope
X" — the canonical pattern (omit from scope or include with a single
named redaction policy) covers the cases the framework needs to
generate. A future amendment can add per-scope redaction overrides if
adopters surface the need; today the design favours the smaller
grammar.

### Runtime expectations

- CRUD writes apply the codec on persistence (Phase 8+).
- Row loading applies the codec on decode (Phase 8+).
- Generated visages, admin defaults, audit-log diff renderers, and
  export bundles all read from the same `ProtectedFieldMetadata`
  source of truth.

The first shipping version does not need a large codec ecosystem. It
only needs one stable contract for how codecs are declared and
discovered — the `FieldCodec` trait + the compile-time registry that
landed in Phase 7.5.

---

## Protected Field Metadata

`FieldDescriptor::protected: Option<ProtectedFieldMetadata>` carries
the parsed annotation. `None` means no `protected(...)` was declared
(which is the common case). The struct lowers to:

```rust
pub struct ProtectedFieldMetadata {
    pub sensitivity: Sensitivity,        // 5-level enum
    pub rationale: Option<&'static str>, // free text
    pub redaction: RedactionPolicy,      // named policy enum
    pub codec: Option<&'static str>,     // codec id; runtime-resolved
    pub retention: RetentionLabel,       // closed enum
}
```

Not every descriptor consumer activates every dimension immediately,
but every consumer reads from the same descriptor source.

These annotations are not application policy by themselves. They are
metadata that later framework surfaces interpret consistently — the
same metadata that drives admin redaction, visage codegen, audit-log
diffs, and export bundle filtering all comes from one source.

---

## Field Codecs

Some fields require a storage transform between the Rust type and the
database representation.

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

The compile-time registry (Phase 7.5 T4) provides the lookup the macro
uses at expansion time to validate `protected(codec = "<id>")`. The
registry is keyed by the codec identifier string and resolved through
the `FieldCodec` trait that adopter code implements. The macro hard-
errors when a `protected(codec = "...")` declaration names an
unregistered identifier — runtime "codec not found" failures are
caught at build time.

---

## Descriptor Contract

`FieldDescriptor` carries enough metadata to answer:

- is this field sensitive? (`protected.sensitivity`)
- in which visages or surfaces may it appear? (`expose(...)` axis,
  separate slot)
- does it use a storage codec? (`protected.codec`)
- what rationale accompanies the protection choice?
  (`protected.rationale`)
- what retention/lifecycle class is associated with it?
  (`protected.retention`)
- how should it be redacted when shown in a redaction-aware surface?
  (`protected.redaction`)

The runtime does not need to execute every one of those dimensions
immediately, but the descriptor shape expresses them all.

---

## Logging & Admin Interaction

Protected-field metadata must influence later framework surfaces:

- admin forms should not accidentally expose protected fields
- shell output should have safe defaults
- audit logs should support redaction-aware diffs
- generated visages should fail if they include disallowed fields

This is why protected-data metadata belongs in Djogi core rather than
in handwritten application DTOs.

---

## Non-Goals

Phase 7.5 does not include:

- legal-hold workflow execution
- purge / anonymize / archive execution
- export bundle delivery
- KMS integration policy
- application-specific authorization

Those belong to later governance phases or app-side code.
