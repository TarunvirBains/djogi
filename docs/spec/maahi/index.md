> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md)

# Maahi — Djogi's Admin Console

Maahi is Djogi's planned optional admin console: an opt-in, descriptor-driven, role-aware UI that is auto-generated from registered models. The intended design is a Dioxus full-stack application running on Axum, but Maahi is a Phase 10 surface and is not shipped in the current `djogi` crate or `djogi` CLI. Every list view, form, filter, validation pass, and audit surface derives from `ModelDescriptor` plus the compile-time visage registry already defined in [Visages](../visages.md) — adopters write zero per-model UI code and zero hand-rolled permission tables.

The name names the planned surface; the feature flag stays generic for discoverability once Maahi ships:

```toml
# Cargo.toml — planned Maahi opt-in, not available in the current release
djogi = { version = "0.1", features = ["admin", "axum"] }
```

| Surface          | Path                                                          |
|------------------|---------------------------------------------------------------|
| Crate            | Planned `djogi-maahi` workspace member, pulled in via `admin` feature when shipped |
| Module           | Planned `djogi::maahi`                                       |
| Feature flag     | Planned `features = ["admin"]`                               |
| URL mount        | `/_admin/`                                                    |
| CLI              | Planned `djogi admin ...` commands; no admin subcommand is registered today |
| Spec             | `docs/spec/maahi/`                                            |

## Design Philosophy

- **Zero hand-written UI.** `ModelDescriptor` carries field names, types, nullability, FK targets, and validation constraints; the compile-time visage registry carries field-set projections. Maahi reads both at runtime and renders the entire UI surface — list views, forms, filters, M2M inlines, JSONB editors — without per-model code.
- **Visages bundle visibility; permissions reference visages.** Visages remain pure compile-time projections (per [Visages](../visages.md) — descriptor data, not authorization). Maahi has its own permission system: roles are granted view/edit access to specific compiled visages via `_admin_role_visage_perms`, and a role's effective field visibility is the union of its granted visages, optionally extended by the `view_full_struct` / `write_full_struct` system permissions, always minus `expose(none)`.
- **Production-grade by default.** Multi-tenancy, visibility-filtered audit log access, server-side write enforcement, CSRF triple stack, session rotation, approval gates on bulk delete — all v1, not deferred. The admin Maahi ships is the admin you'd actually run in production.
- **Pure-Rust end-to-end.** Dioxus full-stack means server functions, hydration, and component tree are all Rust. The same component code is reachable from `dioxus-desktop` for adopters who want a native admin shell.

## Identity, Opt-In, and Phase Boundaries

Maahi is a Phase 10 deliverable. The phase map is:

- **Phase 10 (Maahi v1):** end-to-end admin console — Dioxus renderer, hybrid auth, visage-grant RBAC with single-parent inheritance, six-action permission model with per-model overrides, multi-tenant awareness, compile-time feasibility analysis, visibility-filtered audit log access, four v1 system permissions (`view_audit_log`, `manage_users`, `view_full_struct`, `write_full_struct`), and approval gates on two action kinds: `BulkDelete` (changelist-initiated mass deletion) and `InlineSave` (M2M inline edits at-or-above the inline-bulk threshold), each enforcing approver-coverage of the full action set the package requires.
- **Phase 10.5 (Maahi Compliance & Delegation):** multi-parent role inheritance, frozen/locked roles, the `manage_roles` system permission with transitive upper-bound delegation, approval workflows beyond `BulkDelete` and `InlineSave`, visibility-aware audit retention/redaction. Layered atop Phase 10 without breaking changes.

Phase 10 ships a real admin. Phase 10.5 ships the compliance polish that enterprise deployments need. See [Phase Map](./phase-map.md) for the full deferral list.

The legacy HTMX + Askama renderer described in earlier draft specs is not implemented; the design lineage informed the descriptor-driven philosophy that Maahi keeps. A `djogi-light-admin` (HTMX-only, no WASM toolchain) sits in [`docs/roadmap/future-work.md`](../../roadmap/future-work.md) as a watch-this-space entry, not a planned crate.

## Navigation

| Topic                                        | Document                                          |
|----------------------------------------------|---------------------------------------------------|
| Architecture, auth substrate, multi-tenancy  | [Architecture](./architecture.md)                 |
| Visage-grant RBAC, six-action perms, inheritance, feasibility | [RBAC and Permissions](./rbac.md) |
| CSRF, sessions, server-side write enforcement | [Security](./security.md)                         |
| List views, forms, validation, M2M inlines   | [UI Surface](./ui.md)                             |
| Audit access, system permissions, bulk operations | [Operations](./operations.md)                |
| `expose(none)` floor, `Label` trait, superuser boundaries | [Field Visibility](./field-visibility.md) |
| Sassi-backed caching, cross-runtime predicates, multi-tab invalidation | [Caching and Cross-Runtime State](./caching.md) |
| `[admin]` config block, CLI bootstrap        | [Configuration and CLI](./configuration.md)       |
| Phase 10 deliverables, deferrals, open questions | [Phase Map](./phase-map.md)                  |

---

> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md)
