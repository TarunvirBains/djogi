-- Phase 4 integration fixture: minimal `accounts` table.
--
-- Consumed by `tests/integration/phase4_transactions_expressions.rs` to
-- exercise `atomic()` / savepoints / on_commit drain semantics against
-- live Postgres. Keeps only the columns the Phase 4 Task 1 tests need
-- (`balance` — a bare BIGINT that the tests bump around to prove
-- transactional visibility); richer account attributes can land later
-- if the expression / aggregate tests (Tasks 3-4) reuse the table.
--
-- `generate_id()` is provided by `heeranjid_sqlx::install_schema`, which
-- the Phase 4 test setup helper calls before applying this file, matching
-- the pattern already established by `phase1_model`, `phase2_queryset`,
-- and `phase3_relations`.

CREATE TABLE IF NOT EXISTS accounts (
    id         BIGINT PRIMARY KEY DEFAULT generate_id(),
    balance    BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
