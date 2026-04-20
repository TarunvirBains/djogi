> [Back to Guides](./index.md) · [Back to README](../../ReadMe.MD)

# Transactional Outbox

Phase 4's `#[model(events)]` attribute opts a model into the
transactional outbox pattern. Every `create` / `save` / `delete`
issued through a `DjogiContext` writes a companion outbox row inside
the same transaction — so downstream publishers can read the outbox
table and guarantee "event emitted iff DB change committed".

## Opt in

```rust
use djogi::prelude::*;

#[derive(Model)]
#[model(table = "notifications", events)]
pub struct Notification {
    pub kind: String,
    #[field(outbox = "ignore")]
    pub internal_notes: Option<String>,
}
```

`#[model(events)]` flips `ModelDescriptor::has_outbox` on and makes
the macro emit outbox-writing SQL alongside every mutating CRUD
method.

`#[field(outbox = "ignore")]` strips the annotated column from the
outbox payload — useful for PII, internal scratch fields, or columns
whose values would be noise to downstream consumers.

## Companion table

Each events-model needs a `<table>_outbox` table shaped like:

```sql
CREATE TABLE notifications_outbox (
    id          BIGINT PRIMARY KEY DEFAULT generate_id(),
    row_id      BIGINT NOT NULL,
    action      TEXT NOT NULL,       -- 'create' | 'save' | 'delete'
    payload     JSONB NOT NULL,
    emitted_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    published_at TIMESTAMPTZ
);
CREATE INDEX ON notifications_outbox (published_at) WHERE published_at IS NULL;
```

(DDL side-channel emission to `target/djogi_outbox/*.sql` is deferred
to the Phase 7 migration system; for now, hand-write the outbox table
alongside your own migrations.)

## Semantics

- `Notification::create(ctx, n)` → INSERT the row, INSERT the outbox
  row, both inside `ctx`'s transaction (if any).
- `n.save(ctx)` → UPDATE the row with RETURNING *, then INSERT an
  outbox row whose payload reflects the DB-rehydrated state (so
  trigger-mutated columns surface in the payload).
- `n.delete(ctx)` → DELETE the row, INSERT an outbox row carrying the
  pre-delete snapshot.
- Rollback of `ctx`'s enclosing `atomic()` scope removes both the
  main row change and the outbox row — no half-emitted events.
- Raw sqlx writes (e.g. `sqlx::query(...).execute(&mut *tx).await?`)
  skip the outbox path entirely. The outbox is tied to `DjogiContext`,
  not to the transaction.

## Consumer pattern

Publishers poll the outbox with `WHERE published_at IS NULL`, ship
the rows to their destination (Kafka, SQS, webhook, etc.), and
`UPDATE ... SET published_at = now()` when done. Row locking via
`.skip_locked()` (see the [transactions guide](./transactions.md))
lets multiple publisher workers safely compete for the same table.

```rust
let batch: Vec<NotificationOutbox> = NotificationOutbox::objects()
    .filter(|f| f.published_at().is_null())
    .order_by(|f| f.emitted_at().asc())
    .limit(100)
    .skip_locked()
    .fetch_all(ctx).await?;

for row in &batch {
    ship_to_kafka(&row.payload).await?;
}

NotificationOutbox::bulk_update(
    ctx,
    batch.iter().map(|r| r.id).collect(),
    |f| f.published_at().set(Some(time::OffsetDateTime::now_utc())),
).await?;
```

## Payload policy

The payload is the row serialized to JSON minus any
`#[field(outbox = "ignore")]` columns. `serde::Serialize` must be
derived on the model. The outbox emitter tolerates non-object serde
shapes (tuple structs, enums) and leaves them unfiltered — only
`serde_json::Value::Object` payloads have the exclude filter applied.
