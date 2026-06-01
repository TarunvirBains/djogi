-- Phase 3 integration fixture: `groups_p3` — the other side of the
-- Task 6 many-to-many pair. Symmetric to `persons_p3`; see
-- `005_persons.sql` for the shared rationale.

CREATE TABLE IF NOT EXISTS groups_p3 (
    id BIGINT PRIMARY KEY DEFAULT generate_id(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    name TEXT NOT NULL
);
