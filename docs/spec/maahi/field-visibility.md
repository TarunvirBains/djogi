> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md) | [Maahi](./index.md)

# Maahi — Field Visibility

Field visibility in Maahi is governed by the same `expose(...)` grammar that governs visage generation in [Visages](../visages.md). The grammar defines a finite set of named scopes; each `#[field(expose(...))]` annotation declares which scopes the field belongs to.

## Three Reserved Cases

- **No `expose(...)` on a field** — visible to superuser only. The field is implicitly out of scope for every role.
- **`#[field(expose(none))]`** — invisible to *everyone*, including superuser. Use for password hashes, internal token bytes, anything that must never render in a UI. Superuser bypasses *role* filtering, not data-level hiding.
- **`#[field(expose(public, …))]`** — explicit list of scopes that may see the field, plus superuser implicitly.

```rust
// Field is persisted but never surfaces in any visage, admin included.
#[field(expose(none))]
pub password_hash: String,

// Visible to two specific roles plus superuser.
#[field(expose(billing_agent, support_agent))]
pub email: String,

// No expose() — visible only to superuser.
pub internal_id: String,
```

## Companion Annotations

- **`#[field(admin_readonly)]`** — field renders in forms but is not editable. Independent of visibility; a field can be visible-and-readonly to one scope and visible-and-editable to another by combining `admin_readonly` with the `expose(...)` list.
- **`#[field(sensitive)] #[field(redact_in(admin))]`** — field is rendered as a redacted placeholder rather than its raw value; useful for showing "a value exists" without disclosing it. Defined alongside Phase 7.5's protected-data work; see [Protected Data](../protected-data.md).
- **`#[field(admin_label)]`** — designates this field as the row label for FK dropdowns and list views. If absent, Maahi falls back to the first non-id `String` field on the model.
- **`#[model(admin_label_fn = "Vehicle::admin_label")]`** — opt-in computed label for cases where no single field is right.

## FK Dropdown Feasibility

Foreign-key fields are subject to a feasibility rule: a FK field is exposable to a scope only if the target model has an admin-label-bearing field (declared via `#[field(admin_label)]` or the `String`-fallback) exposed to the same scope. Otherwise the dropdown would render meaningless IDs. Violations surface at startup as feasibility diagnostics (see [RBAC](./rbac.md)).

## Superuser Bypass — Boundaries

Superuser bypasses *role* filtering, not *data-level* hiding. The distinction matters:

| Annotation                          | Visible to role with matching scope | Visible to superuser |
|-------------------------------------|-------------------------------------|----------------------|
| `expose(public)` (when role.scope = public) | Yes                          | Yes                  |
| no `expose(...)`                    | No                                  | Yes                  |
| `expose(none)`                      | No                                  | **No**               |

`expose(none)` is the password-hash invariant: the field is never UI-rendered, regardless of who is asking. Superuser is the operational god mode for the model graph; it is not a security override for data classified as never-render.

## Model-Level Opt-Out

Whole-model opt-out remains available:

```rust
#[model(table = "internal_tokens", admin = false)]
#[derive(Debug, Clone)]
pub struct InternalToken { /* … */ }
```

`admin = false` removes the model from Maahi entirely — no list view, no detail view, no FK dropdown access. The model still participates in the rest of the framework (descriptor, migrations, query API, audit log mirror). The lone effect of `admin = false` is suppressing Maahi's UI surfacing.

---

> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md) | [Maahi](./index.md)
