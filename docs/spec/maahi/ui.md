> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md) | [Maahi](./index.md)

# Maahi — UI Surface

## List Views — Pagination, Search, Filtering, Sorting

Every model's list view paginates by default and exposes per-column sort, full-text search across declared search fields, and per-field filter widgets. All state — page, sort column and direction, search string, filter values, tenant picker — is reflected in the URL query string. List views are bookmarkable and shareable.

Defaults:

- Page size: 25 rows
- Sortable: every column header for which the descriptor reports a sortable type
- Search: case-insensitive `ILIKE` across all `String` fields exposed to the role's scope
- Default sort: most recently created first

Per-model overrides via `#[model(...)]` attributes:

```rust
#[model(
    table = "vehicles",
    admin_list_display     = ["vin", "registration_state", "active"],
    admin_search_fields    = ["vin", "registration_state"],
    admin_filter_fields    = ["active", "fuel_type_id"],
    admin_sort_default     = "created_at",
    admin_sort_default_dir = "desc",
    admin_page_size        = 50,
)]
#[derive(Debug, Clone)]
pub struct Vehicle { /* … */ }
```

Filter widgets auto-typed by field:

| Field type                        | Filter widget                                  |
|-----------------------------------|------------------------------------------------|
| `bool`                            | Toggle: All / True / False                     |
| `Option<T>`                       | Toggle: All / Set / Null                       |
| `ForeignKey<T>`                   | `<select>` populated from `T::objects()` (search-as-you-type for large tables) |
| `DateTime`                        | Date range picker (from / to)                  |
| `i32` / `i64` / `f64`             | Numeric range (min / max)                      |
| `#[field(choices = ...)]`         | Checkbox group of allowed values               |
| `DjogiEnum` (Phase 5)             | Checkbox group of variants                     |
| `Jsonb<T>` subfield               | Per-subfield widget recursively                |

Foreign-key dropdowns for large tables debounce 300 ms client-side and dispatch a server function that returns the top N matching rows; the response respects the requesting role's read access on the target model.

## ModelForms — Field Widget Mapping

Each registered model produces a form view by walking `ModelDescriptor` at runtime. No per-model UI code is hand-written.

| Field type                      | Widget                                                       |
|---------------------------------|--------------------------------------------------------------|
| `String`                        | `<input type="text">`                                        |
| `Option<String>`                | `<input type="text">` with explicit null toggle              |
| `i32` / `i64`                   | `<input type="number">`                                      |
| `f64`                           | `<input type="number" step="any">`                           |
| `bool`                          | `<input type="checkbox">`                                    |
| `DateTime`                      | `<input type="datetime-local">`                              |
| `Date`                          | `<input type="date">`                                        |
| `Option<T>`                     | Widget for `T` plus null toggle                              |
| `ForeignKey<T>`                 | `<select>` with search-as-you-type                           |
| `Jsonb<T>`                      | Nested fieldset from `T`'s schema; unknown fields read-only  |
| `Vec<T>`                        | Repeating fieldset with add/remove controls                  |
| `DjogiEnum`                     | `<select>` of variants (rich variants render as fieldsets)   |
| `GeoPoint` (Phase 6)            | Map widget with click-to-set + manual lat/lng inputs         |
| `#[field(max_length = N)]`      | `maxlength` attribute                                        |
| `#[field(unique)]`              | Client-side hint; uniqueness validated server-side on save   |
| `#[field(admin_readonly)]`      | Rendered as a non-editable display (still visible)           |

## Changeform Validation — `AdminClean`

Before any `INSERT` or `UPDATE` fires, the changeform runs an auto-validation pass derived from `ModelDescriptor`:

- `#[field(max_length = N)]` — length check
- `#[field(unique)]` — pre-write conflict query
- `NOT NULL` fields — error if blank submitted
- `ForeignKey<T>` — verifies the referenced row exists *and* is visible to the requesting role's scope on `T`
- `Jsonb<T>` — runs the `validator` validation tree on the schema before save

Custom validation hooks per model:

```rust
use djogi::maahi::{AdminClean, AdminValidationError};
use djogi::context::DjogiContext;

impl AdminClean for Vehicle {
    async fn clean(&mut self, ctx: &mut DjogiContext) -> Result<(), AdminValidationError> {
        if self.active && self.gas_fill == 0 {
            return Err(AdminValidationError::field(
                "gas_fill",
                "An active vehicle must have fuel.",
            ));
        }
        self.make = self.make.trim().to_string();
        Ok(())
    }
}
```

`AdminClean::clean()` runs after auto-validation passes and before the descriptor-aware save. The `&mut DjogiContext` parameter is intentional — `clean()` needs transactional access to the same context the eventual write uses, including tenant context and on-commit hooks. (The earlier `&PgPool` form in draft specs was a `sqlx`-era artifact; Phase 5-Zero retired `sqlx`.)

Models that do not implement `AdminClean` skip this hook entirely. `AdminValidationError` carries either a field-specific message (rendered next to the offending input) or a form-level message (rendered above the form).

## M2M Through-Table Inlines

When a model declares an M2M relationship (per [Relations](../relations.md)), Maahi renders the through-table as an inline sub-form beneath the main model form. Because Djogi requires explicit through models and `impl ManyToMany<T>` declarations, the admin has all the metadata it needs from descriptors — no extra configuration.

```text
┌─ Person: Alice ──────────────────────────────────┐
│  name:  [Alice                    ]              │
│                                                  │
│  Groups (PersonGroup)                            │
│  ┌──────────────────┬───────────┬──────────────┐ │
│  │ Group            │ Role      │ Joined At    │ │
│  ├──────────────────┼───────────┼──────────────┤ │
│  │ [Engineering  ▼] │ [admin  ] │ [2026-01-01] │ │
│  │ [Product      ▼] │ [member ] │ [2026-03-01] │ │
│  │ [+ Add row      ]                            │ │
│  └──────────────────┴───────────┴──────────────┘ │
│  [ Save ]  [ Delete ]                            │
└──────────────────────────────────────────────────┘
```

- Each inline row uses the same field-to-widget table from above for the through model's fields.
- On save, Maahi diffs the inline state against existing rows and fires the appropriate `create` and `delete` calls in the same transaction as the parent save.
- **Inline create/delete accounting and the approval threshold.** Each inline-row creation or removal is accounted as an individual `Create` or `Delete` action on the through model. A magnitude threshold reroutes mass-removal inline saves through the dual-control approval flow as a distinct `InlineSave` pending action — a sibling to `BulkDelete` that shares the same approval queue, lifecycle, and dual-control discipline (see `operations.md` for the shared table and lifecycle). This closes a bypass: without the threshold, an operator could perform the same volume of deletes via inline editing as via a changelist `BulkDelete` and skip approval.
   - The input is **total inline removals across all M2M relations on a single parent save** (per-save sum, not per-relation — spreading removals across relations does not evade the gate).
   - **Below `[admin].inline_bulk_threshold` (default 25):** normal save flow. Inline diff fires as per-row `Create` / `Delete` calls bundled into the parent's transaction. Requires `Delete` action on the through model. No approval gate.
   - **At or above the threshold:** the parent save is queued as a pending action under `action_kind = 'InlineSave'` in `_admin_pending_actions`; a second administrator approves; on approval, the entire parent save (parent field updates + inline creates + inline updates + inline deletes) executes atomically inside one transaction.
   - **Approver coverage rule (anti-piggyback).** Because an `InlineSave` package bundles heterogeneous operations into one atomic execution, the approver must hold every action permission the package requires — not just `BulkDelete`. The package execution attempts:
       - `Update` on the parent model (if the package modifies parent fields)
       - `Create` on the through model (if the package adds inline rows)
       - `Update` on the through model (if the package edits existing inline rows)
       - `Delete` on the through model (always, since the trigger is removals)
       - `BulkDelete` on the through model (always, by virtue of crossing the threshold)
   - The approval UI surfaces the full action set the package contains, the affected row counts per category, and disables the "Approve" button if the approver lacks any required action, naming the missing permission. This prevents an approver from authorizing operations they couldn't perform themselves: the dual-control safeguard requires *both* operators to cover the full scope of the change, not just the gate-triggering action. As with all approval flows, approver ≠ requester.
   - The submitting operator's permission check happens at submission time and is recomputed at execution time so neither side can drift between submit and approve.
   - The threshold applies only to **removals**. Inline creates and inline updates do not contribute to the threshold; they remain normal-save-flow regardless of count. (High-volume inline-create/update gating is a Phase 10.5 candidate if a deployment needs it.)
   - Changelist-initiated `BulkDelete` continues to follow the standard approval flow defined in `operations.md`. The threshold rule above closes the inline-edit bypass; it does not change changelist behavior.
- Inline pagination — at most `inline_page_size` rows shown (default `10`) with a load-more action; new unsaved rows always render above the paginated set.
- Inline search filters visible rows client-side and re-fetches server-side on page load.
- Inline visibility honors the requesting role's scope on the through model — if the role cannot see the through model at all, the inline is hidden.

Per-relationship configuration on the `ManyToMany` impl:

```rust
impl ManyToMany<Group> for Person {
    type Through = PersonGroup;
    const RELATION: &'static str = "groups";
    const ADMIN_INLINE_PAGE_SIZE: usize = 5;
    const ADMIN_INLINE: bool = true;        // false suppresses the inline entirely
}
```

---

> [Back to README](../../../ReadMe.MD) | [All Specs](../index.md) | [Maahi](./index.md)
