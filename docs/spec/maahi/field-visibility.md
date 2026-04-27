> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md) | [Maahi](./index.md)

# Maahi — Field Visibility

Field visibility in Maahi has two layers, kept deliberately separate:

1. **Data classification — `expose(none)`.** A field marked `#[field(expose(none))]` is invisible to *everyone*, including superuser and including any explicit visage grant. This is the absolute floor and lives at the field annotation, not in any runtime table. Use for password hashes, internal token bytes, anything that must never render in any UI surface. `expose(internal)` is a grammar sentinel equivalent to `expose(none)` per [Visages](../visages.md) — no `{Model}Internal` struct is generated; the floor applies the same way.
2. **Authorization — visage grants.** Every other visibility decision (which roles see which fields, in which form, with what actions) is expressed as visage grants on `_admin_role_visage_perms` (see [RBAC](./rbac.md)). Visages bundle field sets; permissions reference visages.

Visages themselves are pure compile-time projections (see [Visages](../visages.md)). They are descriptor data plus transport-type generators, not a runtime permission system. Maahi consumes them at the visibility-grant boundary; it does not extend their meaning.

## The Three Field Cases

```rust
// Case 1: expose(none) — never UI-rendered, regardless of role or superuser status.
#[field(expose(none))]
pub password_hash: String,

// Case 2: expose(...) — field participates in compiled visages whose names appear
//                       in the list. Per-role visibility is decided by which visages
//                       the role has been granted in _admin_role_visage_perms.
#[field(expose(public, admin))]
pub vin: String,

// Case 3: no expose() — field belongs to no canonical compiled visage. It is
//                        visible to superuser (modulo expose(none)) and to any
//                        non-superuser role holding view_full_struct. To make
//                        such a field visible to a specific role tier, add an
//                        expose(...) annotation pointing at the relevant
//                        canonical scope.
pub make: String,
```

`expose(...)` is the compile-time grammar that drives canonical visage generation. It is *not* the Maahi permission backbone. A role's effective view of a model is computed from the union of fields in its granted visages, optionally plus the full struct via `view_full_struct`, always minus `expose(none)`.

## The `view_full_struct` and `write_full_struct` System Permissions

Holding any number of visage view grants gives a role the *union* of those visages' fields. Seeing the *raw struct* (every field on the model except those marked `expose(none)`) requires the discrete `view_full_struct` system permission.

`write_full_struct` is the parallel grant for write. Holding it lets a role edit any non-`expose(none)` field, independent of which visages they have `can_edit` on. Write implies the corresponding view: granting `write_full_struct` without `view_full_struct` is rejected at write time with a form error suggesting the view grant.

Both permissions stop at the `expose(none)` floor — neither reveals nor allows writes to data-classification-hidden fields. Superuser holds both implicitly. See [Operations](./operations.md) for the full system-permissions table.

## Companion Annotations

- **`#[field(admin_readonly)]`** — field renders in forms but is not editable. Independent of visibility; a visage grant can be `can_edit = TRUE` and the field still render read-only because of this annotation. This is the widget-render axis, not the permission axis.
- **`#[field(sensitive)] #[field(redact_in(admin))]`** — field is rendered as a redacted placeholder rather than its raw value; useful for showing "a value exists" without disclosing it. Defined alongside Phase 7.5's protected-data work; see [Protected Data](../protected-data.md).
- **`#[field(label)]`** — designates this field as the row label source consumed by the model-level `Label` trait. See the section below.
- **`#[model(label_fn = "Vehicle::compute_label")]`** — opt-in computed label for cases where no single field is right.

## The `Label` Trait — Visibility-Aware

Every `#[model]`-annotated struct implements `Label`, a model-level trait — *not* Maahi-specific — that returns a single human-readable string for the row. Used by FK dropdowns, list views, search-result snippets, audit log entries, shell display defaults, and any future surface that needs a one-line row label.

**Critical: `Label` is visibility-aware.** A naively unconditional `label()` would leak hidden field values through FK dropdowns and audit-log views — any caller who can list rows on a model would see labeled fields they have no read permission on. The trait method takes a `VisibleFields` parameter and contractually must not return values from fields outside that set:

```rust
pub trait Label {
    fn label(&self, visible: &VisibleFields) -> String;
}

pub struct VisibleFields { /* sorted set of field names */ }
```

`Label` and `VisibleFields` live in `djogi` (the framework crate), not `djogi-maahi`, so non-admin surfaces (shell, audit, future subapps) consume them without depending on the admin crate. The `#[model]` macro emits the impl using these resolution rules, in priority order, **with every step gated on field visibility**:

1. **`#[model(label_fn = "Vehicle::compute_label")]`** present → the named function is responsible for honoring `VisibleFields` and not returning values from non-visible fields.
2. **`#[field(label)]`** present on a single field F → emitted impl is `if visible.contains("F") { self.F.to_string() } else { fallback() }`.
3. **Fallback search** — first non-id field whose type is `String`-like; same visibility-gated pattern.
4. **Always-safe terminal fallback** — `format!("Vehicle #{}", self.id)`. Always succeeds.

Concurrent presence of `label_fn` and `#[field(label)]` is a compile error — pick one. There is no compile error for "no eligible label field"; rule 4 always succeeds with an ID-only label.

`Label` is intentionally distinct from `std::fmt::Display`. Many models will reasonably have a `Display` impl that's wrong for a UI label (verbose debug-style output, type prefix). `Label` is the dedicated UI-label hook; `Display` stays available for general string formatting and is not visibility-aware.

Maahi consumes `Label` in three places in v1, each constructing `VisibleFields` from the requesting principal's effective visibility on the target model:

- **FK dropdown rendering** — both preload and typeahead tiers display rows by `row.label(&visible)`. Labels are computed per request, so the same row may render different labels for different viewers.
- **List view default column** — when `admin_list_display` is not set on a model, the list view renders the row's `label(&visible)` (with `visible` derived from the requesting role) plus its ID. Custom `admin_list_display` lists honor field-level visibility separately, so the default column path is the only one that touches `Label`.
- **Audit log entries** — `_logs.{model}` rows store JSONB snapshots of the changed source row. When rendering an audit entry to a viewer, Maahi reconstructs the model from the snapshot, computes the *viewer's* effective visibility on the source model (not the actor's at change time — visibility is a property of the reader), and renders the label via `Label`.

Outside Maahi, the Phase 9 shell uses `Label` for `pp()` default rendering — the shell typically constructs `VisibleFields::unrestricted()` since shell users are operator-tier by default, but this is an explicit choice at the call site, not an implicit bypass.

## FK Dropdown Feasibility

A FK field is exposable to a role only if the target model yields a non-empty `Label::label(&visible)` for that role's effective visibility — at minimum the always-safe ID-only fallback always returns *something*, so the practical feasibility check is "is the target model itself visible at all to the role?" If not, the FK dropdown would render a list of unreachable rows. Violations surface at startup as feasibility diagnostics (see [RBAC](./rbac.md), `fk_label_reachable`).

## Superuser Bypass — Boundaries

Superuser bypasses *role* filtering, not *data-level* hiding. The distinction matters:

| Field annotation                      | Visible to role with matching grants | Visible to superuser |
|---------------------------------------|--------------------------------------|----------------------|
| `expose(public)` (role granted view of a visage including this field) | Yes        | Yes                  |
| `expose(...)` but role granted no relevant visage and lacks `view_full_struct` | No | Yes  |
| no `expose(...)`, role lacks `view_full_struct` | No                          | Yes                  |
| `expose(none)`                        | No                                   | **No**               |

`expose(none)` is the absolute floor: the field is never UI-rendered, regardless of who is asking. Superuser is operational god mode for the model graph; it is not a security override for data classified as never-render.

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
