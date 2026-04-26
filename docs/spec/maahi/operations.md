> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md) | [Maahi](./index.md)

# Maahi — Operations

## Audit Log Access

Maahi exposes the CRUD audit log shipped by [Logging](../logging.md), gated by the `view_audit_log` system permission and **scope-filtered** to match the requesting role's scope on the source model.

Granting a role `view_audit_log` does not give that role unrestricted access to every model's `_logs` table. The audit log query layer runs the same field-visibility and tenant filters as the live admin queries: an auditor whose scope allows reading `User.email` and `User.created_at` but not `User.password_hash` will see audit entries for users they can see, with the redacted columns redacted, scoped to their tenant. An "all-or-nothing" audit grant is not the v1 model and never will be — unscoped audit access on a multi-tenant or multi-role deployment is a security hole disguised as a feature.

## System Permissions

Phase 10 system permissions surfaced in `_admin_roles.system_perms`:

| Permission           | What it grants                                                      | Phase |
|----------------------|---------------------------------------------------------------------|-------|
| `view_audit_log`     | Scope-filtered read of `_logs.{model}` tables                       | 10    |
| `manage_users`       | Create/edit/delete `_admin_users`; cannot grant `is_superuser`      | 10    |

`manage_users` carries an upper-bound rule with four coupled clauses. A holder cannot:

1. Grant `is_superuser = TRUE` (only an existing superuser can flip that bit).
2. Assign a role whose `system_perms` are not a subset of the holder's own `system_perms`.
3. Assign a role whose **effective per-(model, action) permission set** — defaults plus `_admin_role_model_perms` overrides, resolved recursively through `parent_role_id` inheritance — is not a subset of the holder's own effective per-(model, action) permission set.
4. **Tenant reach**: assign a role with `cross_tenant = TRUE` unless the holder's own effective authority is also cross-tenant (either `is_superuser = TRUE` or assigned to a role with `cross_tenant = TRUE`). In multi-tenant mode, additionally cannot create or retarget a user into a `tenant_scope` the holder could not themselves operate in — a single-tenant `manage_users` holder can only place users into their own tenant (or NULL when both holder and assigned role are cross-tenant).

Clause 3 closes the per-action escalation surface; clause 4 closes the tenant-reach escalation surface that would otherwise let a single-tenant admin manufacture cross-tenant users. Without clause 4, a `manage_users` holder with the same `(model, action)` matrix as a target role but bounded to one tenant could assign that role with `cross_tenant = TRUE`, creating a user who can act in *every* tenant while the granter can act in only one. With clause 4, a `manage_users` holder can only create users whose realized authority — including tenant reach — is bounded by their own.

This is the same transitive upper-bound discipline that Phase 10.5's `manage_roles` extends to role *editing*; v1 applies it to user *assignment* because the escalation surface is the same.

The `manage_roles` system permission (which extends the transitive upper-bound to role create / edit, not just user assignment) is deferred to Phase 10.5. Until then, role creation and editing are superuser-only operations. Other system actions — running migrations, resetting databases, force-evicting sessions — are also superuser-only in v1.

## Bulk Operations

Bulk update and bulk delete are first-class actions in v1. The list view supports filtered selection, "select all matching", and a bulk-action menu populated with the actions the requesting role's effective `(scope, actions)` pair grants.

`BulkUpdate` and `BulkDelete` cover *changelist*-initiated operations against a filtered or selected row set. They are distinct from per-row `Update` and `Delete` actions in nature. The dual-control approval flow that gates `BulkDelete` is shared with a sibling v1 action kind: M2M inline saves with `inline_bulk_threshold` (default 25) or more total inline removals across all M2M relations on the parent enter the same `_admin_pending_actions` queue as `action_kind = 'InlineSave'` — see `ui.md` for the exact threshold rule. Below the threshold, inline removals fire as per-row `Delete` calls in the parent's save transaction with no approval gate. The two v1 action kinds (`BulkDelete` and `InlineSave`) share the approval queue, lifecycle, and dual-control discipline; they differ in payload shape and which actions the approved package executes. This makes the dual-control approval safeguard inline-edit-aware: mass deletions cannot evade approval by routing through a parent edit form instead of the changelist.

A bulk action is dangerous in proportion to its blast radius. Maahi gates the most dangerous default-on bulk action — `BulkDelete` from a changelist — behind a built-in approval workflow, even in v1, even before the broader approval framework lands.

```sql
CREATE TABLE _admin_pending_actions (
    id              BIGINT PRIMARY KEY DEFAULT generate_id(),
    requested_by    BIGINT NOT NULL REFERENCES _admin_users(id),
    action_kind     TEXT NOT NULL,            -- v1: "BulkDelete" or "InlineSave"; Phase 10.5 extends
    model_name      TEXT NOT NULL,            -- target model: for BulkDelete, the model rows are deleted from;
                                              -- for InlineSave, the parent model whose save is being approved
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
