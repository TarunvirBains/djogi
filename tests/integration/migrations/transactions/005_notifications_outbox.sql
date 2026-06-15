-- Integration fixture: outbox companion for `notifications`.
--
-- `#[field(outbox = "ignore")]` is applied to `internal_notes` on the
-- `Notification` model; the payload-shaping helper in
-- `djogi/src/outbox.rs` strips that column from the JSONB emitted
-- into this table. Macro-side DDL emission is deferred (intended
-- for later development); this file stands in until then.

CREATE TABLE IF NOT EXISTS notifications_outbox (
  id     BIGINT   PRIMARY KEY DEFAULT generate_id(),
  row_id   BIGINT   NOT NULL,
  action   TEXT    NOT NULL,
  payload  JSONB    NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
