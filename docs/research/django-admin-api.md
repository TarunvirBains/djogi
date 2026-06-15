> [Back to README](../../README.md) | [Gap Analysis](../spec/orm-gap-analysis.md)

# Django 6.0 Admin Customization API — Analysis for Djogi HTMX Admin

## Key Design Insight

Django's admin customization has two layers:
1. **Declarative** (attributes on ModelAdmin) — list_display, search_fields, ordering, etc.
2. **Dynamic** (methods on ModelAdmin) — get_queryset, get_readonly_fields, etc. that receive `request` context

Djogi should mirror this: **annotations on the model** for declarative config, **trait impl** for dynamic behavior.

## What Maps to Model Annotations

- `list_display`, `list_display_links`, `list_per_page`
- `search_fields`, `search_help_text`
- `ordering`, `date_hierarchy`
- `list_filter` (auto-detect from field type)
- `readonly_fields`, `fieldsets`
- `save_as`, `save_on_top`
- Per-field: widget type, filter type, prepopulate_from

## What Needs Separate Config (Trait Impl)

- Dynamic `get_queryset` (tenant scoping, user-based filtering)
- Dynamic `get_readonly_fields` (different on add vs change)
- Custom computed display columns (callables)
- Custom actions (bulk operations)
- Custom filters (SimpleListFilter equivalent)
- CRUD hooks (save_model, delete_model overrides)
- Permission hooks (per-object)
- Response flow control

## HTMX-Specific Adaptations

Django's admin assumed full-page loads. HTMX enables:
- **Partial table rendering** — pagination/sort/filter without reload
- **Inline row editing** — `hx-patch` on blur instead of full formset
- **Modal confirmations** — delete/action confirmation as OOB swap
- **Toast notifications** — `HX-Trigger` events instead of Django messages
- **Autocomplete** — `hx-trigger="keyup changed delay:300ms"` on FK selects

## Implementation Priority

**Phase 1 (zero config):** List + CRUD + search + pagination + sorting
**Phase 2 (annotations):** list_display, list_filter, search_fields, ordering, fieldsets, readonly
**Phase 3 (advanced):** Custom actions, inlines, list_editable, custom filters, facets, autocomplete

*Full detailed analysis available in the research agent output.*
