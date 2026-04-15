> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

## 15. Admin Panel — HTMX + Askama

Djogi ships an optional admin panel that auto-generates list views and CRUD forms for every registered model. Built with HTMX + Askama (compile-time templates) and served as server-rendered HTML mounted at `/_admin/` by the Axum router.

### 15.1 Design Philosophy

- **Zero hand-written UI:** `ModelDescriptor` (emitted by `#[derive(Model)]`) carries all the information needed — field names, types, nullability, FK targets, validation constraints. The admin reads this at runtime and renders forms automatically
- **HTMX + server-rendered HTML:** Axum handlers return HTML pages and fragments. HTMX attributes (`hx-get`, `hx-post`, `hx-swap`) handle interactivity — pagination, search-as-you-type, inline editing — without a JS framework or WASM bundle. Just `htmx.min.js` (14KB gzipped).
- **Opt-in, not bundled by default:** Enabled via the `admin` feature flag — adds `askama` to the dependency tree only when explicitly requested
```toml
# Cargo.toml
djogi = { version = "0.1", features = ["admin"] }
```
### 15.2 Mounting the Admin

The admin router is merged automatically at startup when the `admin` feature is active:
```
GET  /_admin/                     → model index (all registered models)
GET  /_admin/{model}/             → list view + pagination + search
GET  /_admin/{model}/add/         → create form
GET  /_admin/{model}/{id}/        → detail / edit form
POST /_admin/{model}/{id}/        → save changes
POST /_admin/{model}/{id}/delete/ → delete with confirmation prompt
```
Access is gated behind configurable admin credentials:
```toml
[admin]
enabled  = true
path     = "/_admin/"
username = "admin"            # or DJOGI_ADMIN_USER env var
# password set via: cargo djogi admin set-password
```
### 15.3 List View — Pagination, Search, Filtering & Sorting

Every model's admin list view is paginated by default. All behaviour is driven from `ModelDescriptor` and tunable per model.

Defaults (zero configuration):
- Paginated at 25 rows per page
- Sortable by any column header (click to sort asc/desc)
- Global search box performs case-insensitive `ILIKE` across all `String` fields

Declarative configuration:
```rust
#[derive(Model)]
#[model(
    table = "vehicles",
    admin_list_display  = ["make", "model_name", "gas_fill", "active"],
    admin_search_fields = ["make", "model_name"],
    admin_filter_fields = ["active", "fuel_type_id"],
    admin_sort_default  = "created_at",
    admin_sort_default_dir = "desc",
    admin_page_size     = 50,
)]
pub struct Vehicle { ... }
```
Filter widgets auto-typed by field:

| Field type | Filter widget |
|---|---|
| `bool` | Toggle: All / True / False |
| `Option<T>` | Toggle: All / Set / Null |
| `ForeignKey<T>` | `<select>` populated by `T::objects()` |
| `DateTime` | Date range picker (from / to) |
| `i32` / `i64` / `f64` | Numeric range (min / max) |
| `#[field(choices = ...)]` | Checkbox group of allowed values |

All filter state, sort, and search are reflected in the URL query string — list views are bookmarkable and shareable.

### 15.4 ModelForms — Field Widget Mapping

For each registered model, the admin generates a form view by iterating `ModelDescriptor` at runtime. No per-model UI code is written by hand.

| Field type | Admin widget |
|---|---|
| `String` | `<input type="text">` |
| `Option<String>` | `<input type="text">` (clearable, nullable) |
| `i32` / `i64` / `f64` | `<input type="number">` |
| `bool` | `<input type="checkbox">` |
| `DateTime` | `<input type="datetime-local">` |
| `Option<T>` | Widget for `T` + explicit null toggle |
| `ForeignKey<T>` | `<select>` populated by `T::objects().fetch_all()` |
| `Jsonb<T>` | Nested fieldset generated from `T`'s schema — unknown fields shown as read-only |
| `#[field(max_length = N)]` | `maxlength` attribute on text input |
| `#[field(unique)]` | Client-side hint; uniqueness validated server-side on save |
### 15.5 Changeform Validation — Clean Before Save

Before any `INSERT` or `UPDATE` fires, the changeform runs a clean pipeline. Errors are surfaced inline, next to the offending field.

Auto-generated field validation (always active, derived from `ModelDescriptor`):
- `#[field(max_length = N)]` — trims to N chars, error if still over
- `#[field(unique)]` — queries DB for conflict before save
- Non-nullable fields — error if blank submitted
- `ForeignKey<T>` — verifies referenced row exists before save
- `Jsonb<T>` — runs `validator` validation on the schema before save

Custom `clean()` hook (opt-in, per model):
```rust
use djogi::admin::AdminClean;

impl AdminClean for Vehicle {
    async fn clean(&mut self, pool: &PgPool) -> Result<(), AdminValidationError> {
        // Cross-field validation
        if self.active && self.gas_fill == 0 {
            return Err(AdminValidationError::field(
                "gas_fill",
                "An active vehicle must have fuel.",
            ));
        }
        // Normalize before save
        self.make = self.make.trim().to_string();
        Ok(())
    }
}
```
`AdminValidationError` carries either a field-specific message or a form-level message. `clean()` runs after auto-validation passes and before `create()` or `save()`. If it returns `Err`, the save is aborted and the form re-rendered — no DB write occurs. Models without `impl AdminClean` skip the hook entirely.
### 15.6 M2M Through-Table Inlines

When editing a model with a `ManyToMany` relationship declared, the admin renders through-table rows as an inline sub-form beneath the main model form. Because Djogi requires explicit through models and `impl ManyToMany<T>` declarations, the admin has all the metadata it needs with zero extra configuration.
```
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
- Each inline row maps the through model's fields to widgets using the same field → widget table
- `ForeignKey<T>` fields render as `<select>` dropdowns
- On save, the admin diffs the inline state against existing rows and fires the appropriate `create`/`delete` calls
- Inline pagination — at most `inline_page_size` rows shown (default: 10) with load-more; new unsaved rows always shown above the paginated set
- Inline search — filters visible rows client-side and fires server-side filtered queries on page load
```toml
[admin]
inline_page_size = 10
```
Configurable per relationship:
```rust
impl ManyToMany<Group> for Person {
    type Through = PersonGroup;
    const RELATION: &'static str = "groups";
    const ADMIN_INLINE_PAGE_SIZE: usize = 5;  // override for this relationship
    const ADMIN_INLINE: bool = true;           // set false to suppress inline
}
```
### 15.7 Opt-Out
```rust
// Exclude a model from the admin entirely
#[derive(Model)]
#[model(table = "internal_tokens", admin = false)]
pub struct InternalToken { ... }

// Hide a specific field (still persisted, not shown)
#[field(admin_hidden)]
pub password_hash: String,
```
### 15.8 Research Areas (Admin)

- Askama template compilation and how `ModelDescriptor` drives template rendering at build time
- `ForeignKey<T>` select dropdowns for large tables — HTMX `hx-trigger="keyup changed delay:300ms"` for search-as-you-type
- Serving `htmx.min.js` as a static asset via `tower_http::services::ServeDir` within the Axum router
- Admin session auth — signed cookie, independent of the application's own auth system
- `Jsonb<T>` unknown fields in admin — read-only display with raw JSON view toggle
