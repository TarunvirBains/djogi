-- Full-Text Search: book table.
--
-- The `search` column is a GENERATED ALWAYS AS tsvector formed by
-- concatenating `title` and `body` through the `english` dictionary.
-- Postgres computes it automatically on every INSERT and UPDATE so
-- application code never writes to it directly.
--
-- The GIN index on `search` is the standard acceleration structure for
-- `@@` (tsvector-matches-tsquery) predicates — GIN indexes are
-- preferred over GiST for static or infrequently-updated text because
-- they produce faster query evaluation at the cost of slightly slower
-- writes.
--
-- `generate_id()` is provided by the `#[djogi_test]` bootstrap which
-- installs the HeeRanjID schema and seeds node 1 before this file runs.

CREATE TABLE IF NOT EXISTS book (
    id          BIGINT PRIMARY KEY DEFAULT generate_id(),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    title       TEXT NOT NULL,
    body        TEXT NOT NULL,
    search      TSVECTOR GENERATED ALWAYS AS (
                    to_tsvector('english', title || ' ' || body)
                ) STORED
);

CREATE INDEX IF NOT EXISTS book_search_gin ON book USING GIN (search);
