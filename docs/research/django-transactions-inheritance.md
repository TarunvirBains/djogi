> [Back to README](../../README.md) | [Gap Analysis](../spec/orm-gap-analysis.md)

# Django 6.0 Transactions & Model Inheritance — Deep Dive for Djogi

## Transaction System

### Key Patterns Djogi Needs

1. **`atomic()` as context manager AND decorator** — Djogi equivalent: `djogi::transaction::atomic()` wrapping sqlx transactions. Supports nesting via savepoints.

2. **`on_commit()` callbacks** — Critical for: sending emails after order saves, cache invalidation after data changes, publishing events after persistence. Callbacks only fire on outermost commit, cleared on rollback.

3. **Savepoint-aware callback tracking** — Each callback records which savepoints were active at registration. On savepoint rollback, callbacks registered within that savepoint are discarded.

4. **`durable=True`** — Guarantee this atomic block corresponds to a real top-level transaction, not a savepoint. Raises error if nested.

5. **`mark_for_rollback_on_error()`** — Lightweight: marks connection for rollback without savepoint overhead. Used for single-query operations.

### Enterprise Use Cases

- Order processing: outer atomic ensures order + items + inventory all commit or roll back
- Payment processing: `durable=True` ensures payment records are never in a savepoint
- Email/notification dispatch: `on_commit()` ensures sent only after committed data
- Cache invalidation: `on_commit()` ensures cache cleared only after persist

## Abstract Models

### Key Design Patterns

1. **Deep copy of fields** — Each child gets its own copy (critical for mutable validator state)
2. **Manager inheritance** — Shallow copy + rebind to child model. First-seen-wins in MRO.
3. **Meta option propagation** — Only `ordering` and `get_latest_by` inherit from concrete parents. Abstract Meta inherits via Python class inheritance.
4. **`%(app_label)s` / `%(class)s` interpolation** — Constraint/index names are formatted per child to avoid collisions
5. **ForeignKey `related_name` interpolation** — `%(class)s_set` gives each child a unique reverse relation name
6. **Diamond inheritance** — Handled via `inherited_attributes` set preventing duplicate field copies

### Enterprise Use Cases

- **Audit trail**: Abstract `Auditable` with `created_at`, `updated_at`, `created_by`
- **Soft delete**: Abstract `SoftDeletable` with `deleted_at` + custom manager
- **Multi-tenant**: Abstract `TenantScoped` with `tenant_id` FK + scoped manager
- **Ordered items**: Abstract `Orderable` with `position` field + interpolated index

### Djogi Recommendation

Abstract models map naturally to Rust **trait-based composition** via attribute-driven opt-ins on `#[model(...)]`. Phase 8α landed the canonical shape: composition opt-ins live as keywords on the model attribute (not as sibling derives) so the model macro can wire them through CRUD and descriptor emission in a single expansion.

```rust
// Phase 8α-final shape (post-T2.4 + T2.6 surface migrations):
#[model(auditable, soft_deletable)]  // adopter declares created_by + deleted_at
                                     // fields; macro emits trait impls and
                                     // (for auditable) the before_create
                                     // populator hook
pub struct Vehicle {
    pub created_by: Option<String>,
    pub deleted_at: Option<OffsetDateTime>,
    // ... other fields
}
```

Earlier Phase 8α drafts proposed sibling derives `#[derive(Auditable)]` / `#[derive(SoftDeletable)]`, but proc macros cannot observe sibling derives — wiring a `before_create` populator from `Auditable` into the model's CRUD path needed the model macro to know about `Auditable` at expansion time. The single-attribute shape resolves that constraint and keeps the two opt-ins symmetric.

## Proxy Models

### What They Enable

- Different managers/ordering on the same table
- Separate admin registrations with different permissions
- Behavioral variants (different methods, serialization)
- Status-based views (`ActiveUser`, `InactiveUser`)

### Djogi Recommendation

Proxy models are valuable for enterprise use. Implement as:

```rust
#[derive(Model)]
#[model(table = "vehicles")]
pub struct Vehicle { ... }

#[derive(Model)]
#[model(proxy_for = "Vehicle", default_order = ["-created_at"])]
pub struct RecentVehicle;  // no new fields, different ordering + manager
```

## Multi-Table Inheritance

**Recommendation: Out of scope.** Too much implicit magic (hidden JOINs, hidden parent saves, hidden CASCADE). Use explicit FK composition instead.

*Full detailed analysis available in the research agent output.*
