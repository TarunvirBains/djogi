-- Integration fixture: attach the BEFORE UPDATE trigger to
-- `notifications`. Requires 006 (function definition) to land first.
--
-- No DROP TRIGGER IF EXISTS because sqlx::migrate applies each file
-- exactly once per migrations table, so the trigger cannot exist
-- when this file runs.

CREATE TRIGGER notifications_rewrite_trigger
  BEFORE UPDATE ON notifications
  FOR EACH ROW
  EXECUTE FUNCTION notifications_rewrite_on_update();
