> [Back to README](../../README.md) | [All Specs](./index.md)

# Data Lifecycle & Governance

Djogi should be able to plan safe data-lifecycle operations from model metadata without owning the surrounding product workflow.

This spec covers lifecycle classes, anonymize/archive/purge planning, and legal-hold primitives.

---

## Goals

- define lifecycle classes at the model and field level
- generate deterministic SQL plans for purge, anonymize, and archive operations
- support legal-hold overrides
- expose operator-facing planning and review commands

Djogi owns the planning layer. Applications or companion crates may still own scheduling, approvals, and notification workflows.

---

## Minimal Public Surface

 should stabilize a planning surface, not a scheduler.

Model/field annotations:

```rust
#[model(lifecycle = "archive")]
#[field(retention_class = "anonymize")]
```

CLI direction:

```text
djogi lifecycle plan
djogi lifecycle show <plan-id>
djogi lifecycle apply <plan-id>
```

Plan capabilities:

- identify affected models and fields
- choose action: purge / anonymize / archive
- respect legal-hold state
- emit audit/event records on apply

The plan format may change internally, but the public operator workflow should remain plan -> review -> apply.

---

## Lifecycle Classes

A model or field may declare a lifecycle class such as:

- `permanent`
- `archive`
- `purge`
- `anonymize`
- `derived_cache`

The exact names may evolve, but the framework needs lifecycle metadata that is explicit rather than scattered through app jobs.

---

## Lifecycle Planning

Djogi should generate plans for:

- row deletion
- field-level anonymization
- archival copy/move workflows
- dependency-aware execution order across related models

These plans should be reviewable before execution.

Example CLI direction:

- `djogi lifecycle plan`
- `djogi lifecycle show`
- `djogi lifecycle apply`

The CLI names are provisional, but the planning/review/apply pattern is required.

---

## Legal Hold

Lifecycle operations need a generic override primitive.

Required concept:

- a row, model family, or lifecycle scope may be placed on hold
- held data is skipped by generated purge/anonymize/archive plans

Legal hold is not product policy. It is a framework-level override that changes generated lifecycle behavior.

---

## Auditability

Lifecycle operations should be auditable.

At minimum:

- lifecycle plans should identify the models/fields affected
- applies should emit audit/event records
- destructive operations should require explicit operator acknowledgement

Lifecycle tooling must compose with Djogi’s existing migration, shell, and logging phases rather than acting as a standalone subsystem.

When logging uses separate databases, lifecycle auditability should continue to work without forcing maintainers to reason about cross-database internals. The intended contract is:

- lifecycle applies write their domain effects to the app database
- lifecycle audit records follow the configured CRUD/event logging policy
- if the active profile is `strict_audit`, lifecycle applies are subject to the same fail-closed CRUD audit requirement as ordinary writes
- event-log emission for lifecycle operations remains best-effort unless an advanced override says otherwise

This keeps lifecycle behavior aligned with the same logging profile maintainers already chose for the rest of the system.

---

## Relationship to Protected Data

Protected-data metadata is a prerequisite.

 assumes earlier phases already define:

- exposure metadata
- redaction metadata
- retention/lifecycle classes

 is where that metadata becomes executable planning and operator tooling.

---

## Non-Goals

This spec does not include:

- application cron scheduling
- user-facing deletion UX
- email or notification workflows
- legal case management
- storage-vendor-specific cold archive adapters

Those belong above Djogi or in companion crates.
