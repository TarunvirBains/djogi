-- Phase 4 integration fixture: minimal `accounts` table.
--
-- Consumed by `tests/integration/phase4_transactions_expressions.rs` to
-- exercise `atomic()` / savepoints / on_commit drain semantics against
-- live Postgres, plus the Task 3a field-vs-field expression test
-- (`balance < overdraft_limit` as an `Expr<bool>` predicate).
--
-- Columns:
-- - `balance`         — the running ledger balance; the Task 1 tests
--   bump this around to prove transactional visibility, the Task 2
--   tests assert rehydration after `save()`, and the Task 3a test
--   compares it against `overdraft_limit`.
-- - `overdraft_limit` — the Task 3a fixture for field-vs-field
--   comparisons. Defaults to 0 so every pre-Task-3a test continues to
--   construct `Account { balance: X, ..Default::default() }` without
--   spelling the new field; `i64::default() == 0` covers the Rust side.
--
-- `generate_id()` is provided by `heeranjid_sqlx::install_schema`, which
-- the Phase 4 test setup helper calls before applying this file, matching
-- the pattern already established by `phase1_model`, `phase2_queryset`,
-- and `phase3_relations`.

CREATE TABLE IF NOT EXISTS accounts (
    id              BIGINT PRIMARY KEY DEFAULT generate_id(),
    balance         BIGINT NOT NULL DEFAULT 0,
    overdraft_limit BIGINT NOT NULL DEFAULT 0,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
