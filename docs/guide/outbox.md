> [Back to Guides](./index.md) · [Back to README](../../README.md)

# Transactional Outbox

's `#[model(events)]` attribute opts a model into the
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

Each events-model gets a framework-owned `<table>_outbox` companion table from
the descriptor projection. `#[djogi::djogi_test(sync_models = [...])]`,
`djogi::testing::sync_models`, and the migration pipeline synthesize the table
whenever `ModelDescriptor::has_outbox` is true. Do not hand-write this table in
ordinary integration tests.

The projected shape is:

```sql
CREATE TABLE notifications_outbox (
 id  BIGINT PRIMARY KEY DEFAULT heerid_next_desc(),
 row_id BIGINT NOT NULL, -- matches the source table PK type
 action TEXT NOT NULL
   CHECK (action IN ('create', 'save', 'delete')),
 payload JSONB NOT NULL,
 created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
 state  TEXT NOT NULL DEFAULT 'pending'
   CHECK (state IN ('pending', 'processing', 'published', 'failed')),
 leased_until TIMESTAMPTZ,
 retry_count INTEGER NOT NULL DEFAULT 0,
 failed_reason TEXT
);
CREATE INDEX ON notifications_outbox (state, created_at)
 WHERE state = 'pending';
```

The `row_id` SQL type follows the source model's primary key type. `HeerId`,
`HeerIdRecencyBiased`, and integer-backed keys use `BIGINT`; `RanjId` and
`RanjIdRecencyBiased` use `UUID`; custom primary keys use their declared
primary-key SQL type.

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
- Raw writes that bypass `DjogiContext` (e.g. running a statement
 directly against an underlying `tokio_postgres::Client`) skip the
 outbox path entirely. The outbox is tied to `DjogiContext`, not to
 the transaction. `ctx.raw_execute(...)` does NOT bypass — it routes
 through the same context plumbing.

## Consumer pattern

Publishers poll the outbox with `WHERE state = 'pending'`, ship
the rows to their destination (Kafka, SQS, webhook, etc.), and
`UPDATE... SET state = 'published'` when done. Row locking via
`.skip_locked()` (see the [transactions guide](./transactions.md))
lets multiple publisher workers safely compete for the same table.

```rust
let batch: Vec<NotificationOutbox> = NotificationOutbox::objects()
.filter(|f| f.state().eq("pending".to_string()))
.order_by(|f| f.created_at().asc())
.limit(100)
.skip_locked()
.fetch_all(ctx).await?;

for row in &batch {
 ship_to_kafka(&row.payload).await?;
}

NotificationOutbox::bulk_update(
 ctx,
 batch.iter().map(|r| r.id).collect(),
 |f| f.state().set("published".to_string()),
).await?;
```

## Payload policy

The payload is the row serialized to JSON minus any
`#[field(outbox = "ignore")]` columns. `serde::Serialize` must be
derived on the model. The outbox emitter tolerates non-object serde
shapes (tuple structs, enums) and leaves them unfiltered — only
`serde_json::Value::Object` payloads have the exclude filter applied.
