> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md)

# Maahi — Djogi's Admin Console

Maahi is Djogi's optional admin console: an opt-in, descriptor-driven, role-aware UI that is auto-generated from registered models. Built as a Dioxus full-stack application, it ships behind the `admin` feature flag and runs on Axum. Every list view, form, filter, validation pass, and audit surface derives from `ModelDescriptor` and the visage scope grammar already defined in [Visages](../visages.md) — adopters write zero per-model UI code and zero hand-rolled permission tables.

The name names the surface; the feature flag stays generic for discoverability:

```toml
# Cargo.toml — Maahi enabled
djogi = { version = "0.1", features = ["admin", "axum"] }
```

| Surface          | Path                                                          |
|------------------|---------------------------------------------------------------|
| Crate            | `djogi-maahi` (workspace member; pulled in via `admin` feature) |
| Module           | `djogi::maahi`                                                |
| Feature flag     | `features = ["admin"]`                                        |
| URL mount        | `/_admin/`                                                    |
| CLI              | `cargo djogi admin …`                                         |
| Spec             | `docs/spec/maahi/`                                            |

## Design Philosophy

- **Zero hand-written UI.** `ModelDescriptor` carries field names, types, nullability, FK targets, validation constraints, and visage scope membership. Maahi reads this at runtime and renders the entire UI surface — list views, forms, filters, M2M inlines, JSONB editors — without per-model code.
- **Visage scopes are the permission backbone.** The same `expose(...)` annotation that drives visage generation drives Maahi's role-based field visibility and write enforcement. There is no parallel permission system; visage scopes ARE the field-visibility unit.
- **Production-grade by default.** Multi-tenancy, scope-filtered audit log access, server-side write enforcement, CSRF triple stack, session rotation, approval gates on bulk delete — all v1, not deferred. The admin Maahi ships is the admin you'd actually run in production.
- **Pure-Rust end-to-end.** Dioxus full-stack means server functions, hydration, and component tree are all Rust. The same component code is reachable from `dioxus-desktop` for adopters who want a native admin shell.

## Identity, Opt-In, and Phase Boundaries

Maahi is a Phase 10 deliverable. The phase map is:

- **Phase 10 (Maahi v1):** end-to-end admin console — Dioxus renderer, hybrid auth, visage-driven RBAC with single-parent inheritance, six-action permission model with per-model overrides, multi-tenant awareness, compile-time feasibility analysis, scope-filtered audit log access, the `manage_users` system permission, and approval gates on two action kinds: `BulkDelete` (changelist-initiated mass deletion) and `InlineSave` (M2M inline edits at-or-above the inline-bulk threshold), each enforcing approver-coverage of the full action set the package requires.
- **Phase 10.5 (Maahi Compliance & Delegation):** multi-parent role inheritance, frozen/locked roles, the `manage_roles` system permission with transitive upper-bound delegation, approval workflows beyond `BulkDelete` and `InlineSave`, scope-aware audit retention/redaction. Layered atop Phase 10 without breaking changes.

Phase 10 ships a real admin. Phase 10.5 ships the compliance polish that enterprise deployments need. See [Phase Map](./phase-map.md) for the full deferral list.

The legacy HTMX + Askama renderer described in earlier draft specs is not implemented; the design lineage informed the descriptor-driven philosophy that Maahi keeps. A `djogi-light-admin` (HTMX-only, no WASM toolchain) sits in [`docs/roadmap/future-work.md`](../../roadmap/future-work.md) as a watch-this-space entry, not a planned crate.

## Navigation

| Topic                                        | Document                                          |
|----------------------------------------------|---------------------------------------------------|
| Architecture, auth substrate, multi-tenancy  | [Architecture](./architecture.md)                 |
| Visage-driven RBAC, six-action perms, inheritance, feasibility | [RBAC and Permissions](./rbac.md) |
| CSRF, sessions, server-side write enforcement | [Security](./security.md)                         |
| List views, forms, validation, M2M inlines   | [UI Surface](./ui.md)                             |
| Audit access, system permissions, bulk operations | [Operations](./operations.md)                |
| `expose(...)` grammar, superuser bypass, labels | [Field Visibility](./field-visibility.md)      |
| `[admin]` config block, CLI bootstrap        | [Configuration and CLI](./configuration.md)       |
| Phase 10 deliverables, deferrals, open questions | [Phase Map](./phase-map.md)                  |

---

> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md)
