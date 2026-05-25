> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# Protected Data Metadata & Field Codecs

Djogi needs a way to describe sensitive fields mechanically and to attach storage/serialization rules that all generated surfaces can respect.

This spec defines the descriptor-level primitives for protected data. It does **not** define full governance execution or legal workflow behavior.


---

## Goals

- mark fields as sensitive in model metadata
- centralize protection semantics in one place
- support field codecs for transformed storage
- let later phases reuse the same metadata for visages, admin, logs, exports, and lifecycle tooling

---

## Minimal Public Surface

Field protection is declared via a single grouped attribute. Five named keys live inside `protected(...)`, and
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
  compile-time codec registry.** The registry is populated with built-in codecs;
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

- **`redaction = "..."`** — named redaction policy:
  - `"none"` — default.
  - `"hash_id"` — hash to opaque identifier; PK-shaped types only
    (rule (d)).
  - `"mask"` — replace with a fixed mask string.
  - `"drop"` — omit the field entirely from redacted renderings.

- **`codec = "..."`** — codec identifier; resolved against the
  compile-time registry.

- **`retention = "..."`** — closed enum of retention/lifecycle labels:
  - `"transient"` — short-lived data.
  - `"standard"` — default retention.
  - `"extended"` — longer-than-default retention.
  - `"archival"` — long-term archival storage.

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

- CRUD writes apply the codec on persistence.
- Row loading applies the codec on decode.
- Generated visages, admin defaults, audit-log diff renderers, and
  export bundles all read from the same `ProtectedFieldMetadata`
  source of truth.

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

The compile-time registry provides the lookup the macro
uses at expansion time to validate `protected(codec = "<id>")`. The
registry is keyed by the codec identifier string and resolved through
the `FieldCodec` trait. **The registry is closed: codecs ship with
the framework, not adopter code.**

---

## Per-Scope Presentation Codecs

### Syntax and Scope Coverage

Presentation codecs allow field values to be transformed differently depending on the visage scope they appear in. Declare them inside the `per_scope` block within `protected(...)`:

```rust
#[field(
    expose(public, support),
    protected(
        sensitivity = "pii",
        rationale = "...",
        per_scope = {
            public = {
                presentation_codec = djogi::presentation::builtins::MaskString
            }
        }
    )
)]
pub email: String,
```

Each entry in `per_scope` maps a scope name to a codec configuration. A scope that is omitted from `per_scope` receives the field's storage type unchanged in that scope's generated visage.

Example: if `email` is `expose(public, support)` with only `per_scope = { public = {...} }`, then:

- `UserPublic::email` has the codec's output type
- `UserSupport::email` is `String` (the storage type)

### `presentation_codec` vs `try_presentation_codec`

Two keys control whether codec application is infallible or fallible:

- **`presentation_codec = Type`** — the codec application is infallible. The generated visage
  for that scope implements `From<&Model>`.
- **`try_presentation_codec = Type`** — the codec application may fail. If any field in a scope
  uses `try_presentation_codec`, the generated visage for that scope implements
  `TryFrom<&Model>` instead of `From<&Model>`.

### Output Type in Generated Visages

When a field carries a codec, the field's type in the generated visage is not the storage type
but the codec's associated output type:

```rust
<CodecType as djogi::presentation::PresentationCodecInfo<StorageType>>::Output
```

This contract is verified at compile time. The macro emits the associated type directly, not
the storage type.

### Identity Queryability Footgun

`djogi::presentation::builtins::Identity` is intentionally permissive:
`QUERYABILITY = PredicateAndOrder`. Treat it as an explicit plaintext opt-in.

If a sensitive field is exposed in a user-facing scope and uses `Identity`,
the generated accessor grants direct predicate/order access on the storage
value through visage query helpers. For PII fields, prefer `MaskString`,
`MaskOptionString`, or HMAC codecs unless plaintext queryability is an
explicitly reviewed requirement.

### HMAC Key Requirement and Startup Validation

HMAC presentation codecs are optional and gated behind the crate feature
`hmac-codec`:

- `djogi::presentation::builtins::HmacSha256HexString`
- `djogi::presentation::builtins::HmacSha256HexOptionString`
- `djogi::testing::install_presentation_hmac_key_for_testing` (`#[doc(hidden)]`, `unsafe`)

When `hmac-codec` is disabled, those symbols are unavailable and no HMAC-key
startup requirement exists for presentation codecs.

When `hmac-codec` is enabled, models that use either HMAC codec require
`DJOGI_PRESENTATION_HMAC_KEY` at startup.

The key must be exactly 64 lowercase hexadecimal characters, which encodes 32 bytes (256 bits)
of entropy for HMAC operations.

`validate_startup_inventory()` invokes each linked codec's
`PresentationCodecInfo::validate_startup()`; only keyed codecs that implement
HMAC validation fail on missing/invalid `DJOGI_PRESENTATION_HMAC_KEY`.

**Pool startup behavior:** `DjogiPool::connect(&database_url)` runs
`validate_startup_inventory()` and returns `Err(DjogiError::PresentationStartup(..))` for
startup validation failures.

**Freestanding validation:** `djogi::presentation::validate_startup_inventory()` performs the
same inventory check without requiring a pool.

**Linked inventory behavior:** both `DjogiPool::connect` and
`validate_startup_inventory()` only validate `PresentationCodecUsage` records that are
linked into the running binary. A binary that links no `#[model]` declarations with
`per_scope` blocks can still return `Ok(())` even when `DJOGI_PRESENTATION_HMAC_KEY`
is missing.

**Test harness pattern:** to exercise startup-failure tests in a unit or integration
binary, include (or link) a model that actually emits a keyed `PresentationCodecUsage`
entry, for example:

```rust
// In the test binary's module path, define:
#[model(table = "startup_inventory_harness")]
#[derive(Debug, Clone)]
struct PresentationStartupHarness {
    #[field(
        expose(public),
        protected(
            sensitivity = "pii",
            rationale = "Harness uses keyed codec for startup validation coverage",
            per_scope = {
                public = {
                    try_presentation_codec = djogi::presentation::builtins::HmacSha256HexString
                }
            }
        )
    )]
    pub email: String,
}
```

Then run `djogi::presentation::validate_startup_inventory()` (or a pool connect path) in the
same binary after installing/removing `DJOGI_PRESENTATION_HMAC_KEY` as needed for the
assertion.

**Testing (`hmac-codec` enabled):** use the doc-hidden `unsafe`
`djogi::testing::install_presentation_hmac_key_for_testing("aabbcc...")`
helper only from a window where no other code in the process is concurrently
reading or writing environment variables, or otherwise satisfies the
platform-specific stronger requirement for env mutation. A mutex that only
serializes `DJOGI_PRESENTATION_HMAC_KEY` is not enough by itself. Wrap the
call in `unsafe` before calling `DjogiPool::connect`. The helper validates
that the key is exactly 64 lowercase hex characters and sets the environment
variable.

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
