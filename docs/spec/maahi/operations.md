> [Back to README](../../../README.md) | [All Specs](../index.md) | [Maahi](./index.md)

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
3. Assign a role whose **effective per-`(app, model, action)` permission set** — defaults plus `_admin_role_model_perms` overrides (keyed `(role_id, app_name, model_name)` per [RBAC](./rbac.md)), resolved recursively through `parent_role_id` inheritance — is not a subset of the holder's own effective per-`(app, model, action)` permission set. The `app_name` qualifier matches the visage-grant axis from clause 4. v1 enforces workspace-wide model-name uniqueness per [Apps and Database Domains](../apps-and-database-domains.md#cross-app-fk-graph-t9) so the subset check is unambiguous on `model_name` alone today, but every grant carries `app_name` to stay forward-compatible with the deferred descriptor-shape change; once that change lands, two apps with a `User` model will resolve independently on both axes, so a holder bounded to `fleet_app` cannot manufacture a user with action authority on `billing_app.User`.
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
    id              BIGINT PRIMARY KEY DEFAULT heerid_next_desc(),
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

## Wire Payload Decoder (Superuser-only)

> **DRAFT — needs review.** This section captures the v1 design plus open questions for security/UX scrutiny. Sassi's binary wire container and entries snapshot export exist as of `sassi 0.1.0-beta.2`; the unsettled parts here are Maahi's operator gating, UX, and visibility/audit treatment.

### Purpose

Maahi exposes an operator-only utility for decoding `sassi` wire-format byte payloads into structured, visibility-filtered output. The v1 payload classes are value-wire records produced by `sassi::wire::to_vec` and consumed by `Punnu::insert_serialized`, plus entries snapshots produced by `Punnu::export_entries_postcard`. The use case is incident response: an operator has bytes from a log, customer report, network trace, backend file, or memory dump, and needs to see what the payload contains against a known visage/cacheable schema. Without this tool, the binary postcard-backed wire leaves operators with no in-Maahi answer to "what was in this payload" — the old JSON envelope's `cat | jq` debuggability does not exist on the current binary wire.

### Sassi wire contract assumed by Maahi

Maahi does not invent a second cache wire format. It decodes the public sassi wire contract:

- `wire::WIRE_FORMAT_MAJOR = 1`.
- Every payload starts with Sassi's fixed binary header: magic prefix, little-endian wire major, kind byte, flags byte, and the cached type name from `Cacheable::cache_type_name`.
- Value-wire records have kind `Value` and a postcard-encoded `T` body.
- Entries snapshots have kind `PunnuEntries` and a body shaped as `<little-endian u32 count> <count x postcard(T)>`. Sassi exports only unexpired L1 entries and sorts them by `T::Id`; TTL deadlines, LRU epochs, and backend state are not part of the snapshot.
- File-backend entries use Sassi's file-entry kind with an expiry prefix ahead of the value body. Maahi may decode them as an incident-response affordance, but it treats expiry metadata as wire metadata rather than as a Maahi field.
- Unsupported reserved kinds such as future "entries with hints" payloads are rejected until Maahi and sassi both specify the operational semantics.

Header validation happens before body decoding. Wrong major, wrong kind, unsupported flags, malformed type names, and `cache_type_name` mismatches produce type-level diagnostics without attempting to interpret postcard bytes.

### Threat model

The decoder is a privilege-escalation primitive in the wrong hands. Decoding raw wire bytes reveals every field of the target visage *as encoded*, before Maahi's normal visibility pipeline ever runs. The doctrine is therefore inverted from the rest of Maahi: the decoder consumes bytes that *contain* every field including `expose(none)` and `expose(internal)` data, and the rendering path is responsible for re-applying the standard visibility filter. The bytes themselves never reach the operator's screen as a hex dump — only the post-filter structured output does.

Specific threats the gating model must defeat:

- **Web-session compromise.** A stolen session cookie (despite the CSRF triple stack from [Security](./security.md)) must not be sufficient to decode payloads. Decode requires a second factor.
- **Cross-tenant peek.** An operator with authority in tenant A pasting bytes that originated in tenant B and decoding against tenant B's schema. The decoder must require a declared tenant context and verify the operator has decode authority *within that tenant*.
- **Forgery via re-encode.** A "decode → edit → re-encode" path turns the decoder into a payload-forgery primitive. Re-encode is explicitly out of scope for this feature; if a future migration-testing tool needs it, it ships as a separately-gated, separately-specced feature.
- **`expose(none)` leak via parse-failure diagnostics.** A naive postcard error message that includes byte context around the failure offset can disclose `expose(none)` data when the visage's struct layout puts a hidden field near the failure point. Diagnostics must be type-level only.

### Gating — Superuser + SSH-key-signed challenge

v1 gates decode on the conjunction of two factors:

1. The operator's `_admin_users` row has `is_superuser = TRUE`.
2. The operator presents a fresh SSH signature over a server-issued challenge, verifiable against the deployment's decode-authorized-keys allow-list.

There is no `decode_wire_payload` system permission in v1 — decode authority is not delegable through the role/visage system. The operation is too privileged to grant without the WebAuthn-shaped story; delegable decode arrives in Phase 10.5 (or whenever Approach C — fresh WebAuthn user-verification — is specced and implemented).

The two-factor structure means:

- A web-session compromise without SSH key access cannot decode. The session by itself is half the answer.
- An SSH key compromise without an active superuser session cannot decode. The key by itself is the other half.
- Single-superuser deployments **can** use decode (unlike `BulkDelete`'s approver ≠ requester rule), because the SSH key is the second factor rather than a peer.

### Challenge content

The server-issued challenge binds the SSH signature to a specific decode operation. Reusing a signature from one decode against a different payload, visage, tenant, or operator must fail signature verification. The challenge is a deterministic encoding of:

- A 32-byte cryptographically-random server nonce.
- The SHA-256 hash of the bytes-to-decode.
- The target visage identifier (`(app, model, visage_name)` triple).
- The declared tenant context (the operator's `current_tenant_scope`, or `null` if the operator is acting cross-tenant under `cross_tenant = TRUE`).
- The operator's `_admin_users.id`.
- An issuance timestamp.

The challenge is valid for a short window (30s feels right for v1; configurable via `[admin].decode_challenge_ttl`). Single-use: the server tracks issued challenges in memory and rejects re-presentation. The `ssh-keygen -Y sign -n maahi-decode-v1` namespace pins the signature semantic so the same SSH key cannot be tricked into producing a signature usable by another deployment of Maahi or by a non-Maahi tool.

### Key allow-list — open question

Three options under consideration; v1 picks one. The decision affects who can add/remove a decode key and what the bootstrap story looks like.

- **Option (a): ops config file.** A path named in `[admin].decode_authorized_keys_path` (mirrors `~/.ssh/authorized_keys` shape). Pure ops config — adding a key is a deployment change, not a Maahi action. Most ops-native, simplest spec, no recursive "who can add keys" question. Cost: changing the allow-list requires deploy access.
- **Option (b): new `_admin_decode_keys` table.** Mutable through Maahi. Most Maahi-native. Recursive gating problem: who can add a decode key? Bootstrap CLI only? Approval queue? Adds spec surface.
- **Option (c): `decode_pubkey` column on `_admin_users`.** Each operator's key tied to their user record. Simple schema, but couples key identity to user identity (rotating a user's SSH key is a `_admin_users` write).

v1 leans Option (a) for its narrowness, but the choice is open. Whatever is picked, key add/remove for v1 is bootstrap-CLI-only — `planned `djogi admin` add-decode-key <pubkey-line>` and the corresponding remove — to avoid the recursive gating problem entirely.

### Operator UX — open question

The signing flow:

1. Operator selects a target visage/cacheable type from a typeahead populated from the `Cacheable` registry (filtered to visages the operator has any view authority on after standard visibility resolution — selecting a fully-hidden visage is pointless).
2. Operator selects the expected wire kind (`Value`, `PunnuEntries`, or, if supported by the deployment, file-entry).
3. Operator pastes bytes into a text input (hex or base64; auto-detected, with whitespace stripped).
4. Operator declares the tenant context (defaults to current `tenant_scope`; `cross_tenant` operators see a tenant picker).
5. Maahi server validates byte size against `[admin].decode_max_bytes` (default 1 MiB v1; oversize attempts logged and rejected before challenge issuance).
6. Maahi server emits the challenge.
7. Operator signs the challenge on their workstation. v1 ships a CLI helper: `planned `djogi admin` sign-decode-challenge --challenge-file <path>` that wraps `ssh-keygen -Y sign -n maahi-decode-v1` and emits the signature. (Stretch: ssh-agent socket integration; not v1.)
8. Operator pastes the signature into Maahi.
9. Maahi server verifies signature, decodes, renders the result through the standard visibility pipeline.

The paste-the-signature step is friction. The CLI helper closes most of it; full agent integration is a future enhancement. v1 accepts the friction as the cost of the second factor.

### Output rendering — standard visibility pipeline

The decoder's output renders through the same visibility filter that audit log entries and live admin queries use:

1. Validate the Sassi binary header against the selected type and wire kind.
2. Decode the postcard body into the target visage's Rust type using `serde::Deserialize`. For `PunnuEntries`, decode the count-prefixed entry sequence and render each entry through the same path.
3. Compute the operator's effective `VisibleFields` for the target model (visage grants resolved across the role chain, plus `view_full_struct` if held — a superuser holds it implicitly), intersected with whatever fields the decoded value actually contains.
4. Render the visible fields as a structured tree. Fields outside the visible set render as `(not visible)`. `expose(none)` fields are absent from the operator's `VisibleFields` set by construction and never render.
5. Sassi wire metadata (wire major, kind, cache type name, entry count for snapshots, and file-entry expiry if present) renders alongside the decoded fields as metadata.

The standard visibility filter applies even though the operator is superuser: `expose(none)` is the absolute floor per [Field Visibility](./field-visibility.md), and the decoder respects it like every other Maahi surface.

### Audit trail

Every decode attempt — successful or failed — writes one row to the operator audit log. Recorded fields:

- `_admin_users.id` of the operator (from session).
- SSH key fingerprint of the signing key (proof of which enrolled credential was used, independent of the user record claim).
- Target visage `(app, model, visage_name)`.
- Declared tenant context.
- SHA-256 of the decoded bytes (so the same payload across two decodes is correlatable; the bytes themselves are not stored).
- Decode result: `success` / `parse_error` / `version_mismatch` / `kind_mismatch` / `type_mismatch` / `unsupported_flags` / `oversize_rejected` / `signature_invalid` / `not_superuser`.
- Timestamp, client IP, user agent.

The audit row never stores the decoded plaintext. The trail establishes "who decoded what against which schema in which tenant," not "what they saw." Decoded plaintext is per-request, in-memory, and not persisted.

### Failure mode diagnostics

Parse failures surface to the operator as type-level diagnostics: "expected `u32` at field offset 3 of `VehicleAdmin`, found EOF." Never a hex dump of bytes around the failure offset, because the bytes may contain `expose(none)` data that is positionally adjacent to the failure point in postcard's encoding.

Wire major mismatch surfaces explicitly: "wire major 0 not supported; deployment expects major 1. The payload may be from beta.1 JSON-era sassi or a future incompatible major." Kind and type-name mismatches are likewise header-level diagnostics: the operator sees which kind/type was presented and which kind/type the selected decoder expected, never a dump of the body bytes.

### Open questions for review

The following are unresolved as of this rough draft and need revisiting before the Phase 10 v3 plan absorbs this section:

- **Q1.** Key allow-list location (Option a/b/c above).
- **Q2.** Whether the SSH-helper CLI subcommand should also exist as an in-Maahi-page "copy this challenge, run this command" affordance, vs. requiring out-of-band CLI use.
- **Q3.** Whether `cross_tenant` operators decoding against a specific tenant should require an extra confirmation step beyond the standard tenant-picker.
- **Q4.** Whether the decoded-result render window has a TTL (e.g., 10 minutes) after which the operator must re-sign to view again, or persists for the session.
- **Q5.** Whether Phase 10 v1 decodes both value-wire records and `PunnuEntries` snapshots, or starts with value-wire records and adds snapshot rendering in 10.5. The sassi substrate supports both; the remaining cost is Maahi UX, pagination, size limits, and field-visibility rendering over multi-entry payloads.
- **Q6.** Whether the failure-mode diagnostic policy ("type-level only, never byte-level") is over-restrictive for legitimate incident-response cases where seeing the byte stream is exactly the diagnostic value. If so, what additional gate (e.g., a separate "raw-bytes" permission requiring its own SSH signature against a stricter challenge) authorizes byte-level inspection.

### v1 → Approach C upgrade path

The SSH-key-match doctrine is intentionally forward-compatible with Approach C (WebAuthn fresh user-verification). The upgrade path:

1. Phase 10 v1 ships SSH-key + superuser as specified above.
2. Phase 10.5 (or a Phase 11+ Maahi compliance milestone) introduces a delegable `decode_wire_payload` system permission with WebAuthn fresh-UV as the second factor (replacing or stacked-with the SSH-key gate, depending on deployment policy).
3. Deployments that want hardware-attested decode without enrolling WebAuthn can substitute hardware-backed SSH agents (`yubikey-agent`, `ssh-tpm-agent`) at the v1 layer — the SSH-key gate verifies signatures regardless of where the private key lives.
4. The audit-trail schema is designed to absorb a `verification_mode` column (`ssh_signature` / `webauthn_uv` / both) without migration, so the upgrade does not invalidate historical audit rows.

The forward-compat discipline means the SSH-key v1 is not throwaway — it remains a valid second-factor for deployments that don't adopt WebAuthn, even after Approach C lands.

---

> [Back to README](../../../README.md) | [All Specs](../index.md) | [Maahi](./index.md)
