> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md) | [Maahi](./index.md)

# Maahi — RBAC and Permissions

## Visages as Visibility Units

Maahi is a runtime authorization system that *consumes* compile-time descriptor metadata; it does not own the field-visibility shape itself. Visages — the compile-time projections defined in [Visages](../visages.md) — remain narrow per their existing spec: they are descriptor data plus transport-type generators. Authorization is an explicit non-goal of visages.

Maahi has its own permission system. Within Maahi, visages are the unit of visibility: a role's view-and-edit access to a model is expressed as a set of *visage grants*, not as per-field grants directly. Engineering owns what fields are visible by writing `expose(...)` annotations on model fields, which drive the canonical visage generation defined in [Visages](../visages.md); operators own which roles can see which visages by checking boxes in the Maahi UI. There is no runtime visage management and no custom-visage definition surface in v1 — visages are exactly the developer-designed views that `expose(...)` produces.

```rust
#[model(table = "vehicles")]
#[derive(Debug, Clone)]
pub struct Vehicle {
    #[field(expose(public, admin))]
    pub vin: String,                    // canonical visage membership

    pub make: String,                   // no expose() — only visible via view_full_struct
                                        //   or to superuser

    #[field(expose(admin))]
    pub registration_state: String,

    #[field(expose(none))]
    pub internal_audit_id: String,      // never visible to anyone, including superuser
}
```

The compiled-visage registry collected at startup for the example above:

```text
fleet_app.Vehicle.VehiclePublic = ["vin"]                          (from expose(public))
fleet_app.Vehicle.VehicleAdmin  = ["vin", "registration_state"]    (from expose(admin))
```

Field-visibility decisions stay under code review — adding a new view tier requires a code change that adds an `expose(...)` annotation on the relevant fields, exactly where security-relevant decisions about what fields go where should live. Operators get expressiveness through the combinatorial space of (canonical visages × role grants × per-model action overrides × the `view_full_struct` / `write_full_struct` system permissions) without bypassing engineering.

Custom user-defined visage structs (a hand-rolled visage with an arbitrary field set, distinct from the canonical visages emitted by `expose(...)`) are deferred to Phase 10.5 — see [Phase Map](./phase-map.md). The deferral keeps Maahi v1 strictly aligned with the existing Phase 4.5 / 7-Zero-2 visage surface, which emits only the canonical visages (`{Model}Public`, `{Model}SelfView`, `{Model}Admin`, `{Model}Export`).

The lone whole-model exception is `#[model(admin = false)]`, which removes a model from Maahi entirely regardless of any visage grant.

## Hierarchy: App → Model → Visage

Visages live in the natural ownership hierarchy from the apps subsystem (Phase 7-Zero):

```
fleet_app/
├── Vehicle/
│   ├── VehiclePublic    (canonical, from expose(public))
│   └── VehicleAdmin     (canonical, from expose(admin))
└── Owner/
    ├── OwnerPublic
    ├── OwnerSelfView
    └── OwnerAdmin

billing_app/
├── Invoice/
│   ├── InvoicePublic
│   └── InvoiceAdmin
└── ...
```

Each visage is fully qualified by `(app_name, model_name, visage_name)`. The qualifier is what permission grants reference. The `(app, model)` half of the qualifier comes from the apps subsystem's app→model ownership rules (Phase 7-Zero); Maahi reads the registry without owning the qualification logic. The `visage_name` half is one of the canonical visage names (`Public`, `SelfView`, `Admin`, `Export`) emitted from `expose(...)` annotations.

Cross-app references are prevented by registry validation: the live compile-time visage registry is keyed by `(app, model, visage_name)` and the role-config write path rejects any triple absent from the registry. A `_admin_role_visage_perms` row referencing `billing_app.User.UserPublic` is invalid because the user model lives in another app's registry entry and the runtime check catches the mismatch on save. The barrier is not purely structural — it is enforced at write time, intentionally, so that schema migrations that re-home a model between apps surface the dangling grants instead of silently rebinding them.

## Roles and Visage-Grant Tables

```sql
CREATE TABLE _admin_roles (
    id              BIGINT PRIMARY KEY DEFAULT generate_id(),
    name            TEXT UNIQUE NOT NULL,
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

    -- System-level grants. Phase 10 ships view_audit_log, manage_users,
    -- view_full_struct, write_full_struct (see operations.md).
    -- Shape: JSONB object mapping permission name to boolean,
    --   e.g. {"view_audit_log": true, "manage_users": false,
    --         "view_full_struct": true, "write_full_struct": false}.
    -- Absent keys default to false. Unknown keys are rejected at write time
    -- against the live registry of valid permission names. Subset comparisons
    -- in the manage_users upper-bound rule (see operations.md) compute
    -- "set of true-valued keys is a subset of the holder's true-valued keys."
    system_perms      JSONB NOT NULL DEFAULT '{}'::JSONB,

    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Per-(role, visage) grants — fully qualified by (app, model, visage_name).
-- This is the primary visibility-grant table.
CREATE TABLE _admin_role_visage_perms (
    role_id      BIGINT NOT NULL REFERENCES _admin_roles(id) ON DELETE CASCADE,
    app_name     TEXT NOT NULL,
    model_name   TEXT NOT NULL,
    visage_name  TEXT NOT NULL,
    can_view     BOOLEAN NOT NULL,
    can_edit     BOOLEAN NOT NULL,
    PRIMARY KEY (role_id, app_name, model_name, visage_name)
);
```

The `(app_name, model_name, visage_name)` triple is validated at write time against the live registry of compiled visages — a compile-time-collected list (via `inventory`) of every canonical visage emitted from `expose(...)` annotations on registered models, qualified by the owning app per the Phase 7-Zero apps-subsystem rules. Granting a role a visage that doesn't exist (typo, removed `expose(...)` annotation, model re-homed to a different app) returns a form error naming the offending qualifier.

`is_superuser` on `_admin_users` bypasses all role filtering: a superuser sees the raw struct on every model (modulo `expose(none)`), every action is granted, every tenant is reachable, every system permission held implicitly. Superuser is provisioned via the bootstrap CLI and explicit `is_superuser = TRUE` flips by an existing superuser. No role row, no visage grant, can promote a user to superuser; that flip never goes through the role surface.

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

Visage perms govern *which fields* the role can see and edit; model perms govern *which actions* the role can take. The two are orthogonal axes: a role with `VehicleAdmin` view+edit grants but no `Update` action on Vehicle can see those fields read-only; a role with `Update` on Vehicle but no view grants on any Vehicle visage cannot see the model at all (and therefore cannot exercise the action).

The admin UI surfaces both flows. "Uniform across model" sets the role-row defaults and writes no per-model rows — the simple case. "Per-model" lets the operator override individual models — the realistic admin case where a support agent reads-and-edits customers but only reads billing.

## Single-Parent Role Inheritance

Single-parent inheritance is supported. `_admin_roles.parent_role_id` introduces a chain; the effective permission set for a role is the recursive union of its own row, its visage grants, and its parent's row + grants, with the child's per-model overrides shadowing the parent's. Cycles are rejected on save. The save dialog shows "this change affects N child roles" before commit. Inheritance display in the role-edit screen distinguishes own permissions from inherited ones with a clear `inherited from <parent>` annotation next to each row.

Multi-parent inheritance, frozen/locked roles, and the transitive upper-bound `manage_roles` system permission are deferred to Phase 10.5 — see [Phase Map](./phase-map.md).

## Effective Permission Resolution

For a request from user `U` against model `M`:

1. Resolve `U`'s effective role chain (self + parent inheritance — single-parent in v1).
2. Compute the effective `(action_bits, per_model_overrides)` from the role chain — gives us "can U Create / Read / Update / Delete / BulkUpdate / BulkDelete on M?"
3. Compute the effective **visage grant set** — union of all `_admin_role_visage_perms` rows across the role chain whose `(app_name, model_name)` matches M.
4. Compute the effective **visible field set on M**:
   - Start with the union of fields across all granted-view visages.
   - If `view_full_struct` is in the effective `system_perms`, add all fields on M except those marked `#[field(expose(none))]`.
   - Subtract any field marked `#[field(expose(none))]` (this floor is absolute).
5. Compute the effective **editable field set on M**:
   - Start with the union of fields across all granted-edit visages.
   - If `write_full_struct` is in the effective `system_perms`, add all fields on M except those marked `#[field(expose(none))]`.
   - Subtract any field marked `#[field(admin_readonly)]` (visible-but-not-editable; widget-render axis).
   - Subtract any field marked `#[field(expose(none))]` (absolute floor).
6. Intersect with feasibility (per the `can_actually_*` analysis below): if the visible field set is empty, the model is invisible to U entirely; if create-required fields aren't in the editable set, Create is denied.

Superuser bypasses steps 1–5 and gets full struct view + edit on every model, modulo `expose(none)`.

The role-config UI surfaces this resolution as a hierarchical checkbox grid: per-app, per-model, per-visage view + edit checkboxes; per-model action overrides; per-role system permission toggles. A `Preview Effects` action walks every model the role can see and shows the resolved field set + action bits — the user-facing surface of the feasibility analysis below.

## Compile-Time Feasibility Analysis

Permission intent and visage grants don't always agree. A role with `can_create = TRUE` on `Vehicle` whose granted visages cover only `vin` cannot actually create a `Vehicle` — `make` is `NOT NULL` without a database default and not in the role's editable field set. Maahi computes this at startup, not at form-submit, and surfaces the result as a diagnostic.

For each `(role, model)` pair, Maahi resolves five feasibilities:

```text
can_actually_read(role, model)   = role.read    AND ≥1 visible field on model
can_actually_update(role, model) = role.update  AND ≥1 visible field is not admin_readonly
can_actually_create(role, model) = role.create  AND visible field set covers all NOT NULL,
                                                     no-database-default fields
can_actually_delete(role, model) = role.delete  AND ≥1 visible field on model
                                   (delete is row-scope, but the model must be visible at all)
fk_label_reachable(role, model, fk_field) =
                                   target_model_of(fk_field) yields ≥1 visible field
                                   under the role's effective visibility on the target
                                   (per the Label rule in field-visibility.md)
```

Bulk actions inherit their per-row counterpart's feasibility plus the bulk bit. The fifth feasibility runs once per FK field on each visible model.

Failures surface at startup as `AppDiagnostic` entries (the diagnostic registry shipped in Phase 7-Zero):

```text
maahi: role 'admin_lite' cannot create Vehicle —
       required field `make` (NOT NULL, no DEFAULT) is not in any granted visage and
       `write_full_struct` is not held. To fix: add `expose(admin)` to `make` so
       VehicleAdmin covers it, grant write_full_struct, or remove Create from this role.

maahi: role 'public_viewer' cannot expose Vehicle.fuel_type_id —
       FK target FuelType has no visible label-bearing field under this role's
       effective visibility on FuelType. To fix: grant a visage on FuelType whose
       canonical field set includes a Label-eligible field, or remove fuel_type_id
       from the granted visage on Vehicle.
```

The corresponding UI affordances (the "New" button on the Vehicle list, the "Save" button on the empty create form, the bulk-create action menu, the `fuel_type_id` FK dropdown) are hidden at render time. Operators discover misconfiguration at deploy time, not when an end user clicks a button that does nothing.

The analysis runs against the `inventory`-collected `ModelDescriptor` registry, the compile-time visage registry, and the `_admin_roles` / `_admin_role_visage_perms` / `_admin_role_model_perms` tables. It re-runs whenever any of those tables is written.

## Visage Drift After Deploy

If a deploy removes a compiled visage that an existing role was granted, Maahi flags the role with an `AppDiagnostic` at startup and treats the missing grant as a no-op (the row stays in `_admin_role_visage_perms` but contributes nothing to resolution). The operator sees an alert in the role-config UI; they remove the dangling row or restore the visage in code. There is no auto-deletion — silently dropping permission rows on deploy would mask intent.

## Role Deletion UX

Role deletion is a real workflow distinct from inheritance edits. Two cascade concerns must be surfaced before a role row is deleted:

- **Assigned users**: deletion is blocked while at least one `_admin_users.role_id` references the role. The role-edit page surfaces the affected user count and requires the operator to reassign or deactivate those users first. Implementation rests on the explicit `ON DELETE RESTRICT` on `_admin_users.role_id` (see `_admin_users` schema in [Architecture](./architecture.md)).
- **Child roles**: deletion is blocked while at least one `_admin_roles.parent_role_id` references this role. The role-edit page shows the affected child-role list; the operator must rewire the children's `parent_role_id` first. The `parent_role_id` FK is also `ON DELETE RESTRICT`.

Visage grants in `_admin_role_visage_perms` cascade-delete via `ON DELETE CASCADE` once the role is removed; reassign-first handles the user and child-role concerns before that point. There is no soft-delete in v1.

---

> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md) | [Maahi](./index.md)
