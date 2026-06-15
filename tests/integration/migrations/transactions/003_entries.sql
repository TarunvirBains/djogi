-- integration fixture: FK child `entries`.
--
-- Child of `ledgers` (see `002_ledgers.sql`). Exists to give the
-- transaction-backed prefetch integration test a parent→child link so
-- `QuerySet::prefetch(...).fetch_all_prefetched(&mut ctx)` can be
-- exercised inside an `atomic(&pool, |ctx| ...)` scope against a live
-- Postgres pool.
--
-- RESTRICT on delete mirrors the framework default for FKs (Djogi's
-- `OnDelete::Restrict` is the default in `#[field(on_delete = ...)]`) —
-- the test does not exercise cascade behavior.

CREATE TABLE IF NOT EXISTS entries (
  id     BIGINT PRIMARY KEY DEFAULT generate_id(),
  ledger_id BIGINT NOT NULL REFERENCES ledgers(id) ON DELETE RESTRICT,
  memo    TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
