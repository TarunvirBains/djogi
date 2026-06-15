-- Integration fixture: minimal `accounts` table.
--
-- Consumed by `tests/integration/transactions_expressions.rs` to
-- exercise `atomic()` / savepoints / on_commit drain semantics against
-- live Postgres, plus field-vs-field expression tests
-- (`balance < overdraft_limit` as an `Expr<bool>` predicate) and
-- CASE-backed UPDATE tests (`status = CASE WHEN ... END`).
--
-- Columns:
-- - `balance`     — the running ledger balance; tests
--  bump this around to prove transactional visibility,
--  assert rehydration after `save()`, and
--  compare it against `overdraft_limit`.
-- - `overdraft_limit` — the fixture for field-vs-field
--  comparisons. Defaults to 0 so every earlier test continues to
--  construct `Account { balance: X, ..Default::default() }` without
--  spelling the new field; `i64::default() == 0` covers the Rust side.
-- - `status`     — the CASE-backed UPDATE fixture. Defaults
--  to empty string so `Account { balance: X, ..Default::default() }`
--  keeps compiling across every earlier test; `String::default()` is
--  the empty string, matching the Postgres default here.
--
-- `generate_id()` is provided by `heeranjid_sqlx::install_schema`, which
-- the test setup helper calls before applying this file, matching
-- the pattern already established by `model`, `queryset`,
-- and `relations`.

CREATE TABLE IF NOT EXISTS accounts (
  id       BIGINT PRIMARY KEY DEFAULT generate_id(),
  balance     BIGINT NOT NULL DEFAULT 0,
  overdraft_limit BIGINT NOT NULL DEFAULT 0,
  status     TEXT NOT NULL DEFAULT '',
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
