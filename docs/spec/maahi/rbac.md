> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md) | [Maahi](./index.md)

# Maahi — RBAC and Permissions

## Visage-Driven Scopes

Maahi reuses the visage scope grammar from [Visages](../visages.md) as its permission backbone. There is no parallel permission system: visage scopes ARE the field-visibility unit, and roles are runtime bindings of `(scope, actions)`.

A field's `expose(...)` annotation defines which scopes can see it. A role names a scope; the scope determines what fields the role can read and write.

```rust
#[model(table = "vehicles")]
#[derive(Debug, Clone)]
pub struct Vehicle {
    #[field(expose(public, support_agent, billing_agent))]
    pub vin: String,

    #[field(expose(billing_agent))]
    pub registration_state: String,

    pub internal_audit_notes: String,       // no expose() → visible only to superuser
    pub make: String,                       // no expose() → visible only to superuser
}
```

**Note:** `superuser` is *not* a listable token inside `expose(...)` — it is an implicit override that bypasses role filtering. Per [Field Visibility](./field-visibility.md), absence of `expose()` makes a field superuser-only by default. Listing `superuser` inside `expose(...)` is rejected by the macro at compile time.

The administrator sees a model only if at least one of its fields exposes to the administrator's scope. A scope that touches no field on a given model means that model is invisible to that role — there is no "model-level allow list" attribute; visibility is implicit and emergent from the field annotations. The lone exception is `#[model(admin = false)]`, which removes a model from Maahi entirely regardless of any field annotation.

## Roles Table

```sql
CREATE TABLE _admin_roles (
    id              BIGINT PRIMARY KEY DEFAULT generate_id(),
    name            TEXT UNIQUE NOT NULL,
    scope           TEXT NOT NULL,                 -- must match a known scope at write time
    parent_role_id  BIGINT REFERENCES _admin_roles(id) ON DELETE RESTRICT,
                                                       -- explicit RESTRICT: deleting a role with child roles
                                                       -- pointing at it is blocked; the role-edit page enforces
                                                       -- rewire-children-first (see "Role Deletion UX" below).
    cross_tenant    BOOLEAN NOT NULL DEFAULT FALSE,

    -- Default actions, used when no per-model override exists.
    can_create        BOOLEAN NOT NULL DEFAULT FALSE,
    can_read          BOOLEAN NOT NULL DEFAULT TRUE,
    can_update        BOOLEAN NOT NULL DEFAULT FALSE,
    can_delete        BOOLEAN NOT NULL DEFAULT FALSE,
    can_bulk_update   BOOLEAN NOT NULL DEFAULT FALSE,
    can_bulk_delete   BOOLEAN NOT NULL DEFAULT FALSE,

    -- System-level grants. Phase 10 ships view_audit_log + manage_users.
    system_perms      JSONB NOT NULL DEFAULT '{}'::JSONB,

    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

Scope names are validated at write time against the live inventory of known scopes (collected via `inventory::iter::<&'static FieldDescriptor>()` and the visage scope registry). Saving a role with an unknown scope returns a form error naming the typo. New scopes always require a code change — the type-safety boundary is at the field annotation, not the database.

`is_superuser` on `_admin_users` bypasses all role filtering: a superuser sees the raw struct on every model, every action is granted, every tenant is reachable. Superuser is provisioned via the bootstrap CLI and explicit `is_superuser = TRUE` flips by an existing superuser. No role row, no scope name, can promote a user to superuser; that flip never goes through the role surface.

## The Six-Action Permission Model

Six actions, not four:

```text
Per-row:  Create, Read, Update, Delete
Bulk:     BulkUpdate, BulkDelete
```

Bulk actions default to per-row capability — if you can update, you can bulk-update — but each is gated by a separate boolean so the dangerous-at-scale bulk paths can be denied independently.

The bulk vs per-row distinction is primarily **operation-origin**: `BulkUpdate` and `BulkDelete` cover changelist-initiated operations against a selected or filtered row set; per-row `Update` and `Delete` cover per-row form operations and small inline-edit saves. M2M inline removals at or above `[admin].inline_bulk_threshold` (default 25 total across all M2M relations on the parent save) route through the dual-control approval flow as a distinct sibling action kind, `InlineSave` (not as `BulkDelete` itself) — `BulkDelete` and `InlineSave` are the two v1 approval action kinds, sharing the same `_admin_pending_actions` queue and lifecycle but distinct in payload shape. Below the threshold, inline removals stay per-row. See `ui.md` for the threshold semantics and `operations.md` for the shared approval mechanics.

## Per-Model Action Overrides

Per-model overrides are first-class, not an extension. The role row carries a `(can_create, …, can_bulk_delete)` *default*; per-model rows shadow that default for specific models:

```sql
CREATE TABLE _admin_role_model_perms (
    role_id           BIGINT NOT NULL REFERENCES _admin_roles(id) ON DELETE CASCADE,
    model_name        TEXT NOT NULL,            -- e.g., "Vehicle"; validated against descriptors
    can_create        BOOLEAN NOT NULL,
    can_read          BOOLEAN NOT NULL,
    can_update        BOOLEAN NOT NULL,
    can_delete        BOOLEAN NOT NULL,
    can_bulk_update   BOOLEAN NOT NULL,
    can_bulk_delete   BOOLEAN NOT NULL,
    PRIMARY KEY (role_id, model_name)
);
```

The admin UI surfaces both flows. "Uniform across scope" sets the role-row defaults and writes no per-model rows — the simple case. "Per-model" lets the operator override individual models — the realistic admin case where a support agent reads-and-edits customers but only reads billing.

## Single-Parent Role Inheritance

Single-parent inheritance is supported. `_admin_roles.parent_role_id` introduces a chain; the effective permission set for a role is the recursive union of its own row and its parent's row, with the child's per-model overrides shadowing the parent's. Cycles are rejected on save. The save dialog shows "this change affects N child roles" before commit. Inheritance display in the role-edit screen distinguishes own permissions from inherited ones with a clear `inherited from <parent>` annotation next to each row.

Multi-parent inheritance, frozen/locked roles, and the transitive upper-bound `manage_roles` system permission are deferred to Phase 10.5 — see [Phase Map](./phase-map.md).

## Effective Permission Resolution

```text
effective_actions(role, model) =
    union(
        recursive_chain(role.parent_role_id).default_actions,
        role.default_actions,
    )
    .overridden_by(_admin_role_model_perms WHERE role_id IN chain AND model_name = model)
    .intersected_with(scope_feasibility(role.scope, model))
```

The last step — intersection with scope feasibility — is where the "user has Update intent but the scope can't see required fields" case is resolved.

## Compile-Time Feasibility Analysis

Permission intent and scope feasibility don't always agree. A role with `can_create = TRUE` on `Vehicle` whose scope sees only `vin` cannot actually create a `Vehicle` — `make` is `NOT NULL` without a database default and not in scope. Maahi computes this at startup, not at form-submit, and surfaces the result as a diagnostic.

For each `(role, model)` pair, Maahi resolves five feasibilities:

```text
can_actually_read(role, model)   = role.read    AND scope has ≥1 visible field on model
can_actually_update(role, model) = role.update  AND ≥1 visible field is not admin_readonly
can_actually_create(role, model) = role.create  AND scope visibility covers all NOT NULL,
                                                     no-database-default fields
can_actually_delete(role, model) = role.delete  AND ≥1 visible field on model
                                   (delete is row-scope, but the model must be visible at all)
fk_label_reachable(role, model, fk_field) =
                                   target_model_of(fk_field) has ≥1 admin-label-bearing
                                   field exposed to role.scope
                                   (per the FK label rule in field-visibility.md)
```

Bulk actions inherit their per-row counterpart's feasibility plus the bulk bit. The fifth feasibility runs once per FK field on each visible model.

Failures surface at startup as `AppDiagnostic` entries (the diagnostic registry shipped in Phase 7-Zero):

```text
maahi: role 'billing_agent_editor' cannot create Vehicle —
       required field `make` (NOT NULL, no DEFAULT) is not in scope `billing_agent`.
       To fix: either expose `make` to `billing_agent`, or remove `Create` from this role.

maahi: role 'support_agent_editor' cannot expose Vehicle.fuel_type_id —
       FK target FuelType has no admin-label-bearing field in scope `support_agent`.
       To fix: expose an admin-label field on FuelType to `support_agent`,
       or remove `fuel_type_id` from `support_agent`'s field set.
```

The corresponding UI affordances (the "New" button on the Vehicle list, the "Save" button on the empty create form, the bulk-create action menu, the `fuel_type_id` FK dropdown) are hidden at render time. Operators discover misconfiguration at deploy time, not when an end user clicks a button that does nothing.

The analysis runs against the `inventory`-collected `ModelDescriptor` registry plus the `_admin_roles` and `_admin_role_model_perms` tables. It re-runs whenever a role row is written.

## Role Deletion UX

Role deletion is a real workflow distinct from inheritance edits. Two cascade concerns must be surfaced before a role row is deleted:

- **Assigned users**: deletion is blocked while at least one `_admin_users.role_id` references the role. The role-edit page surfaces the affected user count and requires the operator to reassign or deactivate those users first. Implementation rests on the explicit `ON DELETE RESTRICT` on `_admin_users.role_id` (see `_admin_users` schema in [Architecture](./architecture.md)).
- **Child roles**: deletion is blocked while at least one `_admin_roles.parent_role_id` references this role. The role-edit page shows the affected child-role list; the operator must rewire the children's `parent_role_id` first. The `parent_role_id` FK is also `ON DELETE RESTRICT`.

There is no soft-delete in v1. Reassign-first is the documented workflow.

---

> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md) | [Maahi](./index.md)
