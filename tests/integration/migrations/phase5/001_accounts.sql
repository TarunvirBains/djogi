-- Phase 5 integration fixture: accounts table with Tracked + version-field columns.
--
-- Consumed by tests/integration/phase5_postgres_native.rs across Task 1
-- (Tracked<T> round-trip), Task 2 (dirty-aware save), and Task 3
-- (optimistic locking via #[field(version)]). Keeping all three tasks on
-- one migration file keeps the schema stable — Task 2 and Task 3 add
-- behavior to save(), not new columns.
--
-- Columns:
-- - `name`      — Tracked<String>. Task 1 round-trips through postgres-types;
--                 Task 2 proves clean Tracked fields are omitted from the
--                 UPDATE SET list.
-- - `balance`   — plain i64. Task 2 proves non-Tracked fields are always
--                 emitted (no behavioral regression for models that do not
--                 opt in to dirty tracking).
-- - `revision`  — i32 with `#[field(version)]`. Task 3 emits
--                 `WHERE id = $n AND revision = $m` + `SET revision = revision + 1`.
--                 Starts at 0; first successful save sets 1.
--
-- `generate_id()` is provided by the `#[djogi_test]` bootstrap which
-- installs the HeeRanjID schema and seeds node 1 before this file runs.

CREATE TABLE IF NOT EXISTS accounts (
    id              BIGINT PRIMARY KEY DEFAULT generate_id(),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    name            TEXT NOT NULL,
    balance         BIGINT NOT NULL DEFAULT 0,
    revision        INTEGER NOT NULL DEFAULT 0
);
