-- accounts table with Tracked columns.
--
-- Consumed by integration tests verifying Tracked<T> behavior,
-- dirty-aware save, and optimistic locking via #[field(version)].
--
-- Columns:
-- - `name`      — Tracked<String>. Round-trips through postgres-types;
--                 verifies clean Tracked fields are omitted from the
--                 UPDATE SET list.
-- - `balance`   — plain i64. Verifies non-Tracked fields are always
--                 emitted (no behavioral regression for models that do not
--                 opt in to dirty tracking).
-- - `note`      — Tracked<Option<String>>. Tests NULL decode
--                 via FromSql override of from_sql_null.
-- - `revision`  — INTEGER NOT NULL DEFAULT 0. Optimistic-lock version
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
