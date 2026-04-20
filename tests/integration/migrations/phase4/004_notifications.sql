-- Phase 4 Task 6 — `Notification` model + companion outbox table.
--
-- `notifications` is the primary table for the `#[model(events)]` test
-- model. Writes through the framework CRUD emit a paired row into
-- `notifications_outbox`; the outbox table has the same DDL shape every
-- events-enabled model gets (see `djogi/src/outbox.rs` for the
-- convention). Macro-side DDL emission is deferred (intended for Phase 6
-- / Phase 7); integration tests hand-write the outbox DDL here until
-- that side-channel lands.
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

-- BEFORE UPDATE trigger that appends " (db-rewritten)" to `kind` on every
-- UPDATE. The `outbox_save_writes_refreshed_payload` integration test
-- relies on this: if the outbox payload came from the pre-save Rust
-- receiver, the value would be `"acknowledged"`; the test asserts
-- `"acknowledged (db-rewritten)"`, which only appears in the column AFTER
-- Postgres runs the trigger during the UPDATE, proving the payload
-- carries DB-refreshed state via `save`'s `RETURNING *` rehydration.
CREATE OR REPLACE FUNCTION notifications_rewrite_on_update()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    NEW.kind := NEW.kind || ' (db-rewritten)';
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS notifications_rewrite_trigger ON notifications;
CREATE TRIGGER notifications_rewrite_trigger
    BEFORE UPDATE ON notifications
    FOR EACH ROW
    EXECUTE FUNCTION notifications_rewrite_on_update();
