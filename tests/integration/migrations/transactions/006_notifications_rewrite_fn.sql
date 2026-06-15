-- Integration fixture: `BEFORE UPDATE` trigger function that
-- appends " (db-rewritten)" to `notifications.kind` on every UPDATE.
--
-- The `outbox_save_writes_refreshed_payload` integration test relies
-- on this: if the outbox payload came from the pre-save Rust
-- receiver, the captured value would be `"acknowledged"`; the test
-- asserts `"acknowledged (db-rewritten)"`, which only appears in the
-- column AFTER Postgres runs this trigger during the UPDATE, proving
-- the payload carries DB-refreshed state via `save`'s `RETURNING *`
-- rehydration.
--
-- Split into its own migration file (and separately from the CREATE
-- TRIGGER in 007) because sqlx::migrate prepares each file as a
-- single statement.

CREATE OR REPLACE FUNCTION notifications_rewrite_on_update()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  NEW.kind := NEW.kind || ' (db-rewritten)';
  RETURN NEW;
END;
$$;
