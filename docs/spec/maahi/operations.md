> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md) | [Maahi](./index.md)

# Maahi — Operations

## Audit Log Access

Maahi exposes the CRUD audit log shipped by [Logging](../logging.md), gated by the `view_audit_log` system permission and **visibility-filtered** to match the requesting role's effective field visibility on the source model (per the resolution rules in [RBAC](./rbac.md)).

Granting a role `view_audit_log` does not give that role unrestricted access to every model's `_logs` table. The audit log query layer runs the same field-visibility and tenant filters as the live admin queries: an auditor whose granted visages on `User` cover `email` and `created_at` but not `password_hash` will see audit entries for users they can see, with the non-visible columns excluded, scoped to their tenant. An "all-or-nothing" audit grant is not the v1 model and never will be — unconstrained audit access on a multi-tenant or multi-role deployment is a security hole disguised as a feature. A holder of `view_full_struct` sees the full struct (modulo `expose(none)` / `expose(internal)`) in audit entries, just as they do in live views; models marked `#[model(admin = false)]` do not surface audit entries through Maahi at all, even for `view_full_struct` holders, since the model is removed from the Maahi UI substrate end-to-end.

Audit-entry rendering reconstructs the source-row model from its `{model}_logs` JSONB snapshot before computing the viewer's visibility and label. Audit-log table lookup resolves on the source model alone — `{snake_case(model)}_logs` per [Logging](../logging.md) §9.1 — under the v1 workspace-wide model-name uniqueness invariant declared in [Apps and Database Domains](../apps-and-database-domains.md#cross-app-fk-graph-t9): two apps cannot share a model short name in v1, so the audit lookup is unambiguous even though `_admin_role_visage_perms` and `_admin_role_model_perms` carry an explicit `app_name` for forward-compat. When that invariant relaxes (per the same section's deferred descriptor-shape change), the audit-log lookup re-keys to `(app, model)` alongside the perms tables. Snapshots may predate the current schema (a field was added, removed, or renamed since the entry was written). Reconstruction tolerates extra and missing fields per the model's existing `Serialize` / `Deserialize` contract; the viewer's effective field set is intersected with whatever the snapshot actually contains, so missing fields render as "(not in snapshot)" and removed-since-snapshot fields surface only if the viewer would currently be permitted to see them on the live model.

## System Permissions

Phase 10 system permissions surfaced in `_admin_roles.system_perms`:

| Permission           | What it grants                                                                                     | Phase |
|----------------------|----------------------------------------------------------------------------------------------------|-------|
| `view_audit_log`     | Visibility-filtered read of `{model}_logs` tables                                                | 10    |
| `manage_users`       | Create/edit/delete `_admin_users`; cannot grant `is_superuser` (full upper-bound rule below)       | 10    |
| `view_full_struct`   | View every field on every model except `expose(none)` / `expose(internal)` — independent of any visage view grant; does not bypass `#[model(admin = false)]` | 10    |
| `write_full_struct`  | Edit every field on every model except `expose(none)` / `expose(internal)` and `admin_readonly` — independent of any visage edit grant; does not bypass `#[model(admin = false)]` | 10    |

`view_full_struct` is the discrete grant for "see everything not data-class-hidden." Holding any number of visage view grants gives a role a *union* of those visages' fields; seeing the *raw struct* requires this discrete grant. Use case: an auditor role that holds `view_audit_log + view_full_struct` on relevant models without holding write permissions.

`write_full_struct` is the parallel grant for write. Use case: a "data operations engineer" role that can fix cross-cutting data issues without being full superuser. Write implies the corresponding view, so granting `write_full_struct` without `view_full_struct` is rejected at write time with a form error — Maahi auto-suggests adding the view grant. The `expose(none)` floor is still absolute; `write_full_struct` cannot reach those fields. Superuser holds both implicitly.

`manage_users` carries an upper-bound rule with five coupled clauses. A holder cannot:

1. Grant `is_superuser = TRUE` (only an existing superuser can flip that bit).
2. Assign a role whose `system_perms` are not a subset of the holder's own `system_perms`. This bounds escalation through `view_full_struct` and `write_full_struct` along with all other system permissions: a holder lacking `write_full_struct` cannot manufacture a user who holds it.
3. Assign a role whose **effective per-`(app, model, action)` permission set** — defaults plus `_admin_role_model_perms` overrides (keyed `(role_id, app_name, model_name)` per [RBAC](./rbac.md)), resolved recursively through `parent_role_id` inheritance — is not a subset of the holder's own effective per-`(app, model, action)` permission set. The `app_name` qualifier matches the visage-grant axis from clause 4 — two apps with a `User` model resolve independently on both axes, so a holder bounded to `fleet_app` cannot manufacture a user with action authority on `billing_app.User`.
4. Assign a role whose **effective visage-grant set** — the union of `_admin_role_visage_perms` rows resolved recursively through `parent_role_id` inheritance, with `can_view` / `can_edit` carried per row — is not a subset of the holder's own effective visage-grant set. The subset check operates per-bit per `(app, model, visage_name)` triple: every `can_view = TRUE` in the assigned role must be matched by `can_view = TRUE` for the same triple in the holder's chain, and likewise for `can_edit`. There is no implicit ordering between visages (no "VehicleAdmin > VehiclePublic"); the only way one grant covers another is the system-permission expansion below. The check is computed against *effective* grants after expansion, so a holder who holds `view_full_struct` trivially satisfies the view side of every specific visage grant on every model (modulo `expose(none)` / `expose(internal)`); same for `write_full_struct` on the edit side. A holder whose own grants cover only `VehiclePublic` view cannot manufacture a user who sees `VehicleAdmin` view, since `VehicleAdmin` is a distinct triple that is not covered by either the holder's specific grants or any held system permission — this prevents the holder from materializing a user who sees fields the holder cannot.
5. **Tenant reach**: assign a role with `cross_tenant = TRUE` unless the holder's own effective authority is also cross-tenant (either `is_superuser = TRUE` or assigned to a role with `cross_tenant = TRUE`). In multi-tenant mode, additionally cannot create or retarget a user into a `tenant_scope` the holder could not themselves operate in — a single-tenant `manage_users` holder can only place users into their own tenant (or NULL when both holder and assigned role are cross-tenant).

Clause 3 closes the per-action escalation surface; clause 4 closes the visibility-grant escalation surface introduced by `_admin_role_visage_perms`; clause 5 closes the tenant-reach escalation surface that would otherwise let a single-tenant admin manufacture cross-tenant users. Without clause 4, a `manage_users` holder whose own grants cover only `VehiclePublic` view could assign a target role granting `VehicleAdmin` view, creating a user who sees registration_state and any other `expose(admin)` fields the granter cannot — exactly the privilege escalation the upper-bound discipline must prevent. Without clause 5, a `manage_users` holder with the same `(model, action)` matrix as a target role but bounded to one tenant could assign that role with `cross_tenant = TRUE`, creating a user who can act in *every* tenant while the granter can act in only one. Together the five clauses bound every axis of authority the new model exposes — system perms, per-model action bits, visage grants, and tenant reach — so a holder can only create users whose realized authority is a subset of their own across every dimension.

This is the same transitive upper-bound discipline that Phase 10.5's `manage_roles` extends to role *editing*; v1 applies it to user *assignment* because the escalation surface is the same. Phase 10.5's `manage_roles` will extend the same five-clause rule to role create / edit so that a delegated role-editor cannot mint visage grants beyond their own.

The `manage_roles` system permission (which extends the transitive upper-bound to role create / edit, not just user assignment) is deferred to Phase 10.5. Until then, role creation and editing are superuser-only operations. Other system actions — running migrations, resetting databases, force-evicting sessions — are also superuser-only in v1.

## Bulk Operations

Bulk update and bulk delete are first-class actions in v1. The list view supports filtered selection, "select all matching", and a bulk-action menu populated with the actions the requesting role's effective `(visibility, actions)` pair grants — visibility resolved from the role's visage grants plus any `view_full_struct` / `write_full_struct` system permissions, actions resolved from the role's defaults plus per-model overrides.

`BulkUpdate` and `BulkDelete` cover *changelist*-initiated operations against a filtered or selected row set. They are distinct from per-row `Update` and `Delete` actions in nature. The dual-control approval flow that gates `BulkDelete` is shared with a sibling v1 action kind: M2M inline saves with `inline_bulk_threshold` (default 25) or more total inline removals across all M2M relations on the parent enter the same `_admin_pending_actions` queue as `action_kind = 'InlineSave'` — see `ui.md` for the exact threshold rule. Below the threshold, inline removals fire as per-row `Delete` calls in the parent's save transaction with no approval gate. The two v1 action kinds (`BulkDelete` and `InlineSave`) share the approval queue, lifecycle, and dual-control discipline; they differ in payload shape and which actions the approved package executes. This makes the dual-control approval safeguard inline-edit-aware: mass deletions cannot evade approval by routing through a parent edit form instead of the changelist.

A bulk action is dangerous in proportion to its blast radius. Maahi gates the most dangerous default-on bulk action — `BulkDelete` from a changelist — behind a built-in approval workflow, even in v1, even before the broader approval framework lands.

```sql
CREATE TABLE _admin_pending_actions (
    id              BIGINT PRIMARY KEY DEFAULT generate_id(),
    requested_by    BIGINT NOT NULL REFERENCES _admin_users(id),
    action_kind     TEXT NOT NULL,            -- v1: "BulkDelete" or "InlineSave"; Phase 10.5 extends
    app_name        TEXT NOT NULL,            -- app qualifier per Phase 7-Zero apps subsystem;
                                              -- matches the (app_name, model_name) qualification axis
                                              -- on _admin_role_visage_perms and _admin_role_model_perms.
    model_name      TEXT NOT NULL,            -- target model: for BulkDelete, the model rows are deleted from;
                                              -- for InlineSave, the parent model whose save is being approved.
                                              -- Validated against the live ModelDescriptor registry under app_name.
    payload         JSONB NOT NULL,           -- shape varies by action_kind (see below)
    requested_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at      TIMESTAMPTZ NOT NULL,     -- pending requests auto-expire
    approved_by     BIGINT REFERENCES _admin_users(id),
    approved_at     TIMESTAMPTZ,
    executed_at     TIMESTAMPTZ,
    rejected_by     BIGINT REFERENCES _admin_users(id),
    rejected_at     TIMESTAMPTZ,
    rejection_note  TEXT
);

CREATE INDEX _admin_pending_actions_unresolved_idx
    ON _admin_pending_actions (requested_at)
    WHERE approved_at IS NULL AND rejected_at IS NULL;
    -- Partial index: supports the pending-queue view (filter on unresolved, order by requested_at).
    -- Only indexes rows actually surfaced; resolved rows accumulate harmlessly outside the index.

CREATE INDEX _admin_pending_actions_expires_at_idx
    ON _admin_pending_actions (expires_at);
    -- Supports the periodic auto-expiry sweep (`expires_at < NOW()`).
```

**v1 ships two action kinds**: `BulkDelete` (changelist-initiated mass deletion) and `InlineSave` (the M2M inline-edit variant created by the threshold rule in `ui.md`). Both share the table, lifecycle, and approver-coverage discipline; they differ in payload shape and which actions the package executes. Phase 10.5 extends with additional kinds (`BulkUpdate` approval, configurable per-action gates).

**Payload shape per action_kind:**

```jsonc
// action_kind = 'BulkDelete'
{
  "filter":  { /* WHERE-clause encoding */ },   // optional
  "row_ids": [ /* explicit row id list */ ]      // optional; one of filter or row_ids must be present
}

// action_kind = 'InlineSave'
{
  "parent_id":      /* parent row id */,
  "parent_updates": { /* changed parent field name -> new value */ },
  "inline_creates": { "<ThroughModel>": [ { /* new row */ }, ... ] },
  "inline_updates": { "<ThroughModel>": [ { "id": <id>, /* changed fields */ }, ... ] },
  "inline_deletes": { "<ThroughModel>": [ <id>, <id>, ... ] }
}
```

A `BulkDelete` issued through the admin UI:

1. Maahi computes the affected row count and confirmation prompt: *type the count to confirm*. A typo here aborts before any approval flow runs.
2. The action is written to `_admin_pending_actions` with `action_kind = 'BulkDelete'`.
3. Notification of the pending action surfaces in the admin's pending-queue view.
4. A second administrator with delete authority on the model approves or rejects.
5. On approval, Maahi re-validates feasibility (rows may have changed) and executes inside a transaction.
6. The full lifecycle is audited.

An `InlineSave` (the variant created by the M2M inline threshold rule in `ui.md`) follows the same lifecycle steps with two adaptations: the payload bundles parent field updates plus inline creates/updates/deletes (per the schema above), and the approver must hold every action permission the package execution requires — Update on the parent (if parent fields change), Create / Update / Delete / BulkDelete on the through model (as the package contains those operations) — not just `BulkDelete`. The approval UI surfaces the full action set with affected row counts per category, and the "Approve" button is disabled when the approver lacks any required action, naming the missing permission. This prevents piggybacking unauthorized mutations onto a delete-only approval; the dual-control safeguard requires both operators to cover the full scope of the change.

Approver cannot equal requester. Pending requests expire after `[admin].pending_action_ttl` (default `24h`) and require resubmission.

**Single-admin deployments cannot satisfy approver ≠ requester.** The bootstrap CLI provisions exactly one superuser. A deployment relying on `BulkDelete` or above-threshold `InlineSave` therefore needs at least two admins — the dual-control safeguard does not relax for single-admin or bootstrap-only state. Operators who need v1 approval-gated action kinds must provision a second admin (superuser or a role with the relevant action authority) before relying on them. See [Configuration](./configuration.md) — the bootstrap flow names this prerequisite explicitly.

The broader application of this mechanism — gating arbitrary destructive actions, configurable approver counts, per-role gating — is Phase 10.5. The schema is designed to absorb that expansion without migration.

`BulkUpdate` in v1 uses the magnitude-confirmation prompt but does not require dual approval. This is a deliberate calibration — the type-the-count step catches the common fat-finger; full approval workflows for `BulkUpdate` are Phase 10.5 territory.

---

> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md) | [Maahi](./index.md)
