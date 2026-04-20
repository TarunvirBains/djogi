-- Phase 4 Task 6 — `Notification` model + companion outbox table.
--
-- `notifications` is the primary table for the `#[model(events)]` test
-- model. Writes through the framework CRUD emit a paired row into
-- `notifications_outbox`; the outbox table has the same DDL shape every
-- events-enabled model gets (see `djogi/src/outbox.rs` for the
-- convention). Phase 7's migration differ will emit this DDL from the
-- macro side-channel; for now the integration tests hand-write it.
--
-- `internal_notes` is the `#[field(outbox = "ignore")]` column — the
-- payload-shaping helper strips it from the JSONB emitted into the
-- outbox, so downstream consumers never see its contents.

CREATE TABLE IF NOT EXISTS notifications (
    id           BIGINT      PRIMARY KEY DEFAULT generate_id(),
    created_at   TIMESTAMPTZ NOT NULL    DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL    DEFAULT now(),
    kind         TEXT        NOT NULL,
    internal_notes TEXT
);

CREATE TABLE IF NOT EXISTS notifications_outbox (
    id         BIGINT      PRIMARY KEY DEFAULT generate_id(),
    row_id     BIGINT      NOT NULL,
    action     TEXT        NOT NULL,
    payload    JSONB       NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
