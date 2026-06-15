-- integration fixture: FK target `ledgers`.
--
-- Parent table for the transaction-backed prefetch integration test in
-- `tests/integration/transactions_expressions.rs`. Pairs with
-- `003_entries.sql` (child table with a non-null FK into this one) to
-- exercise the generalised prefetch loader inside an `atomic()` scope.
--
-- Scoping note: this file ships here because it is *test*
-- schema, not application schema. machinery will be introduced; 
-- for now, integration tests issue this DDL
-- manually during test setup.

CREATE TABLE IF NOT EXISTS ledgers (
  id     BIGINT PRIMARY KEY DEFAULT generate_id(),
  name    TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
