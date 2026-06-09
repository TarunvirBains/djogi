-- integration fixture: `groups` — the other side of the
-- many-to-many pair. Symmetric to `persons`; see
-- `005_persons.sql` for the shared rationale.

CREATE TABLE IF NOT EXISTS groups (
    id BIGINT PRIMARY KEY DEFAULT generate_id(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    name TEXT NOT NULL
);
