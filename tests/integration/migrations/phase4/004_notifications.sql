-- Phase 4 Task 6 — primary `notifications` table for the
-- `#[model(events)]` outbox integration tests. The companion outbox
-- table and trigger live in separate migration files (005, 006, 007)
-- because sqlx::migrate prepares each file as a single statement —
-- multi-statement files fail with SQLSTATE 42601
-- ("cannot insert multiple commands into a prepared statement").

CREATE TABLE IF NOT EXISTS notifications (
    id             BIGINT      PRIMARY KEY DEFAULT generate_id(),
    created_at     TIMESTAMPTZ NOT NULL    DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL    DEFAULT now(),
    kind           TEXT        NOT NULL,
    internal_notes TEXT
);
