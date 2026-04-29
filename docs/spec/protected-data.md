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
grouped attribute. Five named keys live inside `protected(...)`, and
`sensitivity` is **mandatory**:

```rust
#[field(protected(
    sensitivity = "pii",                   // REQUIRED — see vocabulary below
    rationale   = "GDPR Art. 6(1)(b)",     // REQUIRED when sensitivity > "none"
    redaction   = "mask",                  // optional; default "none"
    codec       = "<see codec section>",   // optional; default unset
    retention   = "extended",              // optional; default "standard"
))]
pub email: String,
```

`rationale`, `redaction`, `codec`, and `retention` are optional keys
within `protected(...)`. The grammar is order-insensitive and rejects
duplicate keys with a span-precise error.

### Validation rules (compile-time)

The macro enforces five rules at expansion time. Violations produce
span-precise compile errors at the offending key, not at the model
struct.

- **(a) `sensitivity` is mandatory.** Omitting it errors with
  `protected(...) requires sensitivity = "..."`.
- **(b) `sensitivity = "none"` is incompatible with any other key.**
  If `none` is paired with `rationale` / `redaction` / `codec` /
  `retention`, the macro errors at the first extra key with a "drop
  this key or raise sensitivity" pointer. The intent: the neutral-
  default form has no metadata to carry, so writing `protected(...)`
  at all is meaningless when sensitivity is `none`.
- **(c) Sensitivity above `none` requires non-empty `rationale`.**
  An empty string also fails. The rationale is the audit trail's
  primary signal — without it the annotation is hostile to compliance
  review.
- **(d) `redaction = "hash_id"` is only valid on PK-shaped types.**
  Specifically `HeerId`, `RanjId`, their family aliases
  (`HeerIdDesc` / `HeerIdRecencyBiased` / `RanjIdDesc` /
  `RanjIdRecencyBiased`), and `Option<...>` of any of the same.
  Adopter custom-PK newtypes from `djogi::primary_key!` are NOT
  recognised by this rule today — the macro cannot prove a user-named
  ident implements `PrimaryKey` at parse time, and a wrong accept
  ships an unsafe redaction policy at runtime, so the recogniser is
  conservative. Custom-PK support for this rule is a deferred
  capability tied to a later phase that gives the macro full
  descriptor-pass visibility.
- **(e) `codec = "..."` must name a value in the framework's
  compile-time codec registry.** Phase 7.5 ships an empty registry —
  every codec string is rejected at expansion time with
  `unregistered codec ID 'X'. Valid codec IDs in this build of
  Djogi: (none).` The registry will be populated in future phases;
  **codecs ship with the framework, not adopter code.**

### Field annotation vocabulary

- **`sensitivity = "..."`** — five-level enum:
  - `"none"` — default, no sensitivity; cannot be combined with other
    `protected(...)` keys.
  - `"internal"` — internal-only data (logs, internal admin, etc.).
  - `"pii"` — personally identifying information.
  - `"sensitive"` — sensitive but not regulated as PII.
  - `"secret"` — credentials, encryption keys, etc.

- **`rationale = "..."`** — free text. Required when sensitivity is
  above `"none"` (see rule (c)).

- **`redaction = "..."`** — named redaction policy. Phase 7.5 ships:
  - `"none"` — default.
  - `"hash_id"` — hash to opaque identifier; PK-shaped types only
    (rule (d)).
  - `"mask"` — replace with a fixed mask string.
  - `"drop"` — omit the field entirely from redacted renderings.

- **`codec = "..."`** — codec identifier; resolved against the
  compile-time registry. Phase 7.5 registry is empty (rule (e)).

- **`retention = "..."`** — closed enum of retention/lifecycle labels.
  Phase 7.5 ships:
  - `"transient"` — short-lived data.
  - `"standard"` — default retention.
  - `"extended"` — longer-than-default retention.
  - `"archival"` — long-term archival storage.

  Future labels (e.g. `legal_hold`, `anonymize`) are spec amendments,
  not adopter extensions.

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

### Flat `rationale` attribute

The parser ALSO accepts `#[field(rationale = "...")]` outside the
`protected(...)` list — it lives in the macro's `VALID_FIELD_KEYS`
allowlist alongside the other field-level keys. **Phase 7.5 does not
propagate the flat-key value into the descriptor**: the descriptor
emitter hard-codes `FieldDescriptor.rationale: None` for every field.
Treat the flat key as parser-accepted-but-not-lowered today; a future
phase that adds a non-protected rationale slot to the descriptor will
wire it through.

For now, every adopter-visible rationale comes from
`ProtectedFieldMetadata::rationale`, populated only via the grouped
`protected(rationale = "...")` form.

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
    pub sensitivity: Sensitivity,    // 5-level enum
    pub rationale: &'static str,     // free text; "" when absent (set only via protected(...))
    pub redaction: RedactionPolicy,  // named policy enum
    pub codec: Option<&'static str>, // codec id; runtime-resolved
    pub retention: RetentionLabel,   // closed enum
}
```

The `rationale` slot is a bare `&'static str` (not `Option<...>`) — the
macro emits the empty string `""` when absent. This matches the
descriptor's "non-empty when sensitivity > none" invariant: rule (c)
above blocks expansion if a non-`none` sensitivity carries empty
rationale, so by the time a `ProtectedFieldMetadata` is constructed
either `sensitivity == None` (in which case `rationale == ""` is
correct by the spec's neutral-default semantics) or
`sensitivity != None` (in which case the macro guaranteed non-empty
text). Consumers reading the field can treat `""` as "no rationale
recorded" without an `Option` peel.

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
the `FieldCodec` trait. **The registry is closed: codecs ship with
the framework, not adopter code.** Phase 7.5 ships the registry empty
(rule (e) above) — every `codec = "..."` declaration is rejected at
expansion time. Future framework phases will populate the registry
with the canonical codec set; adopters who need a custom transform in
the meantime work around it at the application layer.

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
