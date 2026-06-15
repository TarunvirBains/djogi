# Bulk upsert: INSERT...SELECT ON CONFLICT

`QuerySet::insert_into(...)` copies rows from one model's query result into
another table. Adding `ON CONFLICT` turns that copy into a bulk upsert.

## When to use this vs MERGE

| Need | Use |
|------|-----|
| Insert-or-skip on a unique key | `on_conflict_do_nothing` |
| Insert-or-update on a unique key | `on_conflict_do_update` |
| Conditional update | `on_conflict_do_update_where` |
| Delete or flag rows missing from the source | `merge_into` |
| Multiple conditional branches | `merge_into` |

## DO NOTHING

```rust,ignore
use djogi::prelude::*;

CompletedOrder::objects()
    .insert_into::<OrderArchive, _, _>(|t, s| vec![
        t.original_id().copy_from(s.id().as_insert_source()),
        t.total().copy_from(s.total().as_insert_source()),
    ])
    .on_conflict_do_nothing(ConflictTarget::columns([OrderArchive::fields().original_id()]))
    .execute(&mut ctx)
    .await?;
```

## DO UPDATE and EXCLUDED

```rust,ignore
PageViewBatch::objects()
    .insert_into::<DailyTotal, _, _>(|t, s| vec![
        t.day().copy_from(s.day().as_insert_source()),
        t.hits().copy_from(s.hits().as_insert_source()),
    ])
    .on_conflict_do_update(
        ConflictTarget::columns([DailyTotal::fields().day()]),
        |t| vec![t.hits().conflict_set_expr(
            t.hits().as_conflict_expr() + t.hits().excluded().into_conflict_expr(),
        )],
    )
    .execute(&mut ctx)
    .await?;
```

## updated_at is not auto-stamped

`DO UPDATE SET` updates **exactly** the columns you list — nothing more.
Unlike `Model::save`, it does not bump `updated_at` for you: the column
`DEFAULT` only fires on the INSERT path, never on the conflict-update
path. If you want the conflicting row's `updated_at` refreshed, assign it
explicitly in the update closure:

```rust,ignore
.on_conflict_do_update(
    ConflictTarget::columns([DailyTotal::fields().day()]),
    |t| vec![
        t.hits().conflict_set_expr(
            t.hits().as_conflict_expr() + t.hits().excluded().into_conflict_expr(),
        ),
        // Stamp the update time yourself — nothing else will.
        t.updated_at().conflict_set_value(OffsetDateTime::now_utc()),
    ],
)
```

This is deliberate: the SET list has no hidden columns, matching djogi's
explicit-over-magic design.

## Conditional updates

```rust,ignore
.on_conflict_do_update_where(
    ConflictTarget::columns([Doc::fields().slug()]),
    |t| vec![
        t.body().conflict_set(t.body().excluded()),
        t.version().conflict_set(t.version().excluded()),
    ],
    |t| t.version().excluded().conflict_gt(t.version()),
)
```

When the guard is false, Postgres skips the conflicting row instead of
updating it.

## Nullable conflict helpers

Nullable columns can stay on the typed surface for the two common upsert
patterns that previously required either a non-nullable proxy column or a
raw SQL escape hatch.

Use `conflict_is_null()` / `conflict_is_not_null()` when the `DO UPDATE ...
WHERE` guard should depend on whether the existing target value is present:

```rust,ignore
.on_conflict_do_update_where(
    ConflictTarget::columns([Profile::fields().slug()]),
    |t| vec![t.avatar_url().conflict_set(t.avatar_url().excluded())],
    |t| t.avatar_url().conflict_is_null(),
)
```

Use `conflict_coalesce_excluded()` when the SET expression should keep the
existing non-NULL target value and only fall back to the incoming
`EXCLUDED` value when the target is NULL:

```rust,ignore
.on_conflict_do_update(
    ConflictTarget::columns([Profile::fields().slug()]),
    |t| vec![
        t.avatar_url()
            .conflict_set_expr(t.avatar_url().conflict_coalesce_excluded()),
    ],
)
```

These helpers are intentionally gated to `Option<T>` columns. The emitted
SQL is the direct PostgreSQL form:

- `conflict_is_null()` -> `target.col IS NULL`
- `conflict_is_not_null()` -> `target.col IS NOT NULL`
- `conflict_coalesce_excluded()` -> `COALESCE(target.col, EXCLUDED.col)`

Remember that `DO UPDATE ... WHERE` still follows PostgreSQL three-valued
logic: only `TRUE` performs the update; both `FALSE` and `NULL` skip it.
The null-test helpers let you make that behavior explicit instead of
depending on a nullable comparison result.

## RETURNING

`execute_returning(...)` composes with `ON CONFLICT`. `DO NOTHING` omits
skipped rows from `RETURNING`; `DO UPDATE` includes updated rows.
