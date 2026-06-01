-- Phase 4 integration fixture: FK target `ledgers_p4`.
--
-- Parent table for the transaction-backed prefetch integration test in
-- `tests/integration/phase4_transactions_expressions.rs`. Pairs with
-- `003_entries.sql` (child table with a non-null FK into this one) to
-- exercise the generalised prefetch loader inside an `atomic()` scope —
-- the Phase 4 Task 1 closure requires prefetch to work over the
-- transaction-backed `ContextInner::Transaction` variant, not just
-- pool-backed contexts.
--
-- `_p4` suffix keeps this table namespaced away from the Phase 1/2/3
-- integration fixtures so all phases' tests can share one database
-- without DDL collisions.

CREATE TABLE IF NOT EXISTS ledgers_p4 (
    id         BIGINT PRIMARY KEY DEFAULT generate_id(),
    name       TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
