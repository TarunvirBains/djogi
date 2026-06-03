-- Phase 5 integration fixture: accounts table with Tracked columns.
--
-- Consumed by tests/integration/postgres_native.rs across Task 1
-- (Tracked<T> round-trip), Task 2 (dirty-aware save), and Task 3
-- (optimistic locking via #[field(version)]).
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
-- - `revision`  — INTEGER NOT NULL DEFAULT 0. Task 3: optimistic-lock version
--                 counter. The macro emits `revision = revision + 1` in the
--                 SET list and `AND revision = $n` in the WHERE clause on
--                 every save(). Zero-row RETURNING maps to LockConflict.
--
-- `generate_id()` is provided by the `#[djogi_test]` bootstrap which
-- installs the HeeRanjID schema and seeds node 1 before this file runs.

CREATE TABLE IF NOT EXISTS accounts (
    id              BIGINT PRIMARY KEY DEFAULT generate_id(),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    name            TEXT NOT NULL,
    balance         BIGINT NOT NULL DEFAULT 0,
    note            TEXT NULL,
    revision        INTEGER NOT NULL DEFAULT 0
);
