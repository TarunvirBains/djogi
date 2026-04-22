-- Phase 5 integration fixture: accounts table with Tracked columns.
--
-- Consumed by tests/integration/phase5_postgres_native.rs across Task 1
-- (Tracked<T> round-trip) and Task 2 (dirty-aware save).
-- Task 3 and beyond will extend this schema with additional columns as needed.
--
-- Columns:
-- - `name`      — Tracked<String>. Task 1 round-trips through postgres-types;
--                 Task 2 proves clean Tracked fields are omitted from the
--                 UPDATE SET list.
-- - `balance`   — plain i64. Task 2 proves non-Tracked fields are always
--                 emitted (no behavioral regression for models that do not
--                 opt in to dirty tracking).
-- - `note`      — Tracked<Option<String>>. Task 1 extended: tests NULL decode
--                 via FromSql override of from_sql_null.
--
-- `generate_id()` is provided by the `#[djogi_test]` bootstrap which
-- installs the HeeRanjID schema and seeds node 1 before this file runs.

CREATE TABLE IF NOT EXISTS accounts (
    id              BIGINT PRIMARY KEY DEFAULT generate_id(),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    name            TEXT NOT NULL,
    balance         BIGINT NOT NULL DEFAULT 0,
    note            TEXT NULL
);
