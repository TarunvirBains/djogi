-- outbox worker test table.
--
-- A standalone `worker_outbox` table that exercises the full worker-side
-- schema (state machine columns, lease, retry tracking). This is a fresh
-- test fixture rather than altering the `notifications_outbox`
-- table so the integration behavior is preserved unchanged.
--
-- State machine: pending → processing → (published | failed).
--  - published: terminal success.
--  - failed: terminal failure (retry budget exhausted or non-retryable error).
--  - processing → pending: recover_stale resets rows with expired leases.
--  - failed → pending: NOT done by the framework; operators can manually
--   reset rows after fixing the root cause.
--
-- Columns:
--  id      BIGINT PRIMARY KEY — HeerId, populated by generate_id().
--  row_id    BIGINT NOT NULL — FK-like reference to the source row's PK.
--  action    TEXT NOT NULL — 'create', 'save', or 'delete'.
--  payload    JSONB NOT NULL — serialised source row payload.
--  created_at  TIMESTAMPTZ NOT NULL DEFAULT now().
--  state     TEXT NOT NULL DEFAULT 'pending' — state machine column.
--  leased_until TIMESTAMPTZ NULL — set when state='processing'; cleared on
--         transition back to pending or terminal states.
--  retry_count  INT NOT NULL DEFAULT 0 — incremented on each retryable failure.
--  failed_reason TEXT NULL — most recent error message; set by mark_failed.
--
-- An index on (state, created_at) accelerates the FOR UPDATE SKIP LOCKED
-- sub-SELECT in claim_pending, which filters on state='pending' and orders
-- by created_at.

CREATE TABLE IF NOT EXISTS worker_outbox (
  id      BIGINT   PRIMARY KEY DEFAULT generate_id(),
  row_id    BIGINT   NOT NULL,
  action    TEXT    NOT NULL,
  payload    JSONB    NOT NULL,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  state     TEXT    NOT NULL DEFAULT 'pending',
  leased_until TIMESTAMPTZ NULL,
  retry_count  INT     NOT NULL DEFAULT 0,
  failed_reason TEXT    NULL
);

CREATE INDEX IF NOT EXISTS worker_outbox_state_created_at_idx
  ON worker_outbox (state, created_at);
