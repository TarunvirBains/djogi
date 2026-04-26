> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md) | [Maahi](./index.md)

# Maahi — Phase Map and Anchored Deferrals

## Phase 10 — Maahi v1

Phase 10 ships a real, production-grade admin:

- Dioxus full-stack renderer on Axum, `cargo djogi admin build` integration
- Hybrid `_admin_users` / `_admin_sessions` substrate in the audit DB
- Visage-grant RBAC with single-parent inheritance — `_admin_role_visage_perms` per `(role, app, model, visage)`; visages remain pure compile-time projections
- Six-action permission model with per-model overrides
- Multi-tenant aware login, session, and query path
- Compile-time feasibility analysis surfaced as `AppDiagnostic` entries
- Visage-drift handling on deploy (missing visage diagnostics, dangling-grant tolerance)
- Triple-stack CSRF + session rotation discipline + server-side write enforcement
- List view, ModelForm, M2M inline, `AdminClean`, JSONB nested editor — full descriptor-driven UI surface
- Visibility-aware `Label` trait + `VisibleFields` substrate (lives in `djogi`, consumed by Maahi for FK dropdowns, list view default columns, and audit log entries)
- Four v1 system permissions: `view_audit_log` (visibility-filtered audit access), `manage_users` (five-clause upper-bound covering `is_superuser`, `system_perms` subset, per-(model, action) subset, visage-grant subset, and tenant-reach), `view_full_struct` (raw-struct read independent of visage grants), `write_full_struct` (raw-struct write; requires `view_full_struct`)
- `_admin_pending_actions` table + approval gates on two action kinds in v1:
  - `BulkDelete` — changelist-initiated mass deletion
  - `InlineSave` — M2M inline edits at or above `[admin].inline_bulk_threshold` (default 25 total inline removals across the parent save), with approver-coverage of the full action set the package requires
- Magnitude-confirmation prompt on `BulkUpdate`
- Field-visibility primitives: `expose(none)` floor, `admin_readonly`, `#[field(label)]` / `#[model(label_fn = "...")]` for the `Label` trait
- Bootstrap CLI: `set-password --superuser`, `reset-password`, `build`, `info`

## Phase 10.5 — Maahi Compliance & Delegation

Phase 10.5 layers compliance polish atop Phase 10 without breaking changes:

| Deferral                                                                | Reason                                                         |
|-------------------------------------------------------------------------|----------------------------------------------------------------|
| Multi-parent role inheritance (diamond resolution rules)                | Substantial conflict-resolution design; thin slice of value    |
| Frozen / locked roles                                                   | Defensive feature; cascade-impact UX in v1 covers most cases   |
| `manage_roles` system permission with transitive upper-bound delegation | Subtle correctness; deserves dedicated escalation-path testing |
| Approval workflows beyond `BulkDelete` and `InlineSave` (configurable per action/model) | v1 mechanism exists; broadening surface area is its own design |
| Approval-queue UX polish (per-role notifications, bulk approval)        | Layered atop v1 single-action approval                         |
| Visibility-aware audit retention and redaction                          | Builds on v1 visibility-filtered read access                   |
| Django parity — `list_select_related` (FK eager-loading on list view; auto-detect from `admin_list_display`) | Performance at scale: without it, FK columns on the list view trigger N+1 queries |
| Django parity — `raw_id_fields` equivalent (third FK widget tier above typeahead — no-widget-just-ID with popup search) | For huge target tables where even typeahead query cost is wasteful |
| Django parity — `fields` / `fieldsets` (explicit form-field ordering and grouped sections) | Usability for complex models; v1 renders fields in declaration order with no visual grouping |
| Django parity — `AdminAction` extension trait for custom bulk actions    | Beyond `BulkUpdate` / `BulkDelete`; adopter-defined named actions ("Mark as published", "Send welcome email") |
| Django parity — per-row history view (audit-log drill-down for a single record) | Natural fit given v1's visibility-filtered audit log substrate; ships as a UI affordance + drill query |
| Django parity — `list_editable` (inline-edit columns from list view)     | Power-user editing surface; complements full-form editing |
| Django parity — `prepopulated_fields` (auto-populate fields from other fields) | Common admin pattern (slug from title); requires lightweight client-side coordination |
| Django parity — `date_hierarchy` (date drill-down on list views)         | Operator UX for date-heavy models; adds a navigation surface above the filter widgets |
| Django parity — inline polish (`extra`, `min_num`, `max_num`, per-relation `can_delete`) | M2M inline form refinements: default empty rows, min/max enforcement, per-relation delete suppression |
| Django parity — `view_on_site` (link from admin row to public URL)       | Adopter-driven; can also land via an extension trait rather than core |

## Notification Infrastructure — Slot TBD

A separate phase, position in the roadmap dependent on adoption demand. The slot reservation is real; the number is not.

| Deferral                                                                | Reason                                                         |
|-------------------------------------------------------------------------|----------------------------------------------------------------|
| `djogi::email::EmailSender` trait + reference SMTP impl behind feature flag | Framework-level dep, not Maahi-specific                    |
| Email-driven forgot-password flow                                       | Consumer of `EmailSender`                                      |
| Email-driven approval notifications                                     | Consumer of `EmailSender`; replaces in-admin-only queue        |

Maahi v1 ships with operator-side CLI fallbacks where notification flows would otherwise live (e.g., `cargo djogi admin reset-password <email>` instead of an email-driven self-service flow).

## Open Questions

- **Bundle delivery in production.** `cargo djogi admin build` produces a WASM bundle; whether Djogi releases ship a pre-bundled artifact for adopters who don't run a build pipeline is open. Pre-bundling tightens release coupling but reduces adoption friction.
- **Component re-use across `dioxus-desktop`.** The renderer choice keeps the desktop path open. Whether Phase 10 publishes a desktop-shell crate, leaves it to adopters, or formally defers to a follow-up phase is open.
- **List-view streaming for large result sets.** Pagination is fine for thousands of rows; tens of millions argues for cursor-based streaming with virtualized rendering. v1 defaults to offset pagination; cursor mode is an open question.
- **Inline diff display on edits.** Showing "what changed" alongside a save confirmation is a quality-of-life feature with non-trivial implementation cost (capturing the pre-image, rendering the diff). Deferred unless there's demand.
- **Extension hooks.** Adopters may want to inject custom fields, custom actions, or custom navigation entries. v1 is descriptor-driven end-to-end. The shape of an extension surface — `impl AdminExtension for MyApp`, or a registration macro, or descriptor side-channels — is open.

These are intentionally left open in v1 so the Maahi implementation has room to discover the right shapes without freezing them prematurely.

---

> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md) | [Maahi](./index.md)
