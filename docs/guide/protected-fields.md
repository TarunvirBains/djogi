# Protected Fields (Data Governance)

Djogi provides first-class support for data governance through the `#[field(protected(...))]` attribute. This metadata allows you to declare the sensitivity, purpose, redaction policy, and encryption requirements for individual fields directly in your model.

## Overview

Data governance metadata serves several purposes in Djogi:
1. **Discovery:** Makes PII and sensitive data discoverable via `ModelDescriptor`.
2. **Audit Trail:** Requires a rationale for collecting sensitive data, aiding in GDPR and compliance audits.
3. **Redaction:** Enforces automatic redaction in public-facing [Visages](./visages.md).
4. **Encryption:** Triggers transparent [Encryption at Rest](./encrypted-at-rest.md) for secrets.
5. **Retention:** Labels data for downstream retention and deletion policies.

## Attribute Grammar

All governance metadata is nested under the `protected(...)` attribute. Top-level attributes like `#[field(sensitive = "...")]` are **not supported** and will be ignored or rejected by the parser.

### Complete Example

```rust
use djogi::prelude::*;

#[model(table = "users")]
#[derive(Debug, Clone)]
pub struct User {
    // PII field with sensitivity + redaction + retention
    #[field(protected(
        sensitivity = "pii",
        rationale = "Email confirmation flow — GDPR Art. 6(1)(b)",
        redaction = "mask",
        retention = "extended"
    ))]
    pub email: String,

    // Explicit neutral sensitivity (no other keys allowed)
    #[field(protected(sensitivity = "none"))]
    pub status: Status,

    // Encrypted secret with archival retention
    #[field(protected(
        sensitivity = "secret",
        rationale = "Legacy recovery codes",
        codec = "aes256_gcm_v1",
        retention = "archival"
    ))]
    pub recovery_code: Option<String>,
}
```

### Supported Keys

| Key | Valid Values | Required? | Default | Notes |
|-----|-------------|-----------|---------|-------|
| `sensitivity` | `"none"`, `"internal"`, `"pii"`, `"sensitive"`, `"secret"` | **Yes** | - | `"none"` cannot be combined with any other keys. |
| `rationale` | any string | Yes* | - | Required when `sensitivity` is higher than `"none"`. |
| `redaction` | `"none"`, `"hash_id"`, `"mask"`, `"drop"` | No | `"none"` | `"hash_id"` is only valid on `HeerId`/`RanjId` fields. |
| `codec` | `"aes256_gcm_v1"` | No | `None` | Triggers encryption at rest. See [Encrypted at Rest](./encrypted-at-rest.md). |
| `retention` | `"transient"`, `"standard"`, `"extended"`, `"archival"` | No | `"standard"` | Used for data lifecycle management. |
| `per_scope` | `{ scope = { ... } }` | No | - | Declares [Presentation Codecs](./models.md#protected-fields-and-presentation-codecs) for visages. |

## Validation Rules

Djogi enforces several rules at compile time to ensure your governance metadata is coherent:

- **Sensitivity vs. Extra Keys:** If you set `sensitivity = "none"`, you cannot provide any other keys (rationale, redaction, codec, etc.). Either remove the `protected(...)` attribute entirely or raise the sensitivity level.
- **Mandatory Rationale:** Any field with sensitivity higher than `"none"` must provide a non-empty `rationale`. This ensures your codebase contains the "why" behind sensitive data collection.
- **Codec Registry:** The `codec` value must match a registered encryption codec (currently only `"aes256_gcm_v1"` is shipped).
- **HashID Compatibility:** `redaction = "hash_id"` can only be used on fields whose type is `HeerId`, `RanjId`, or their variants. Using it on a `String` or `Integer` will trigger a compile-time error.

## Redaction Policies

When a field is projected into a [Visage](./visages.md), the `redaction` policy determines how the value is handled if no specific `per_scope` codec is provided:

- `"none"`: The value is projected as-is (plaintext).
- `"mask"`: The value is replaced with a standard mask (e.g., `[REDACTED]`).
- `"drop"`: The field is omitted or set to `None`/empty in the visage.
- `"hash_id"`: The ID is hashed (useful for obfuscating database IDs in public URLs).

## Presentation Codecs

For fine-grained control over how protected fields appear in different visages (e.g., masking emails in a "support" view but hashing them in a "public" view), use the `per_scope` key.

See the [Models Guide](./models.md#protected-fields-and-presentation-codecs) for detailed examples of `per_scope` usage.
