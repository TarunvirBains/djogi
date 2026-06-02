-- Phase 5 Task 7: extend posts table with a boolean column for bool_and /
-- bool_or aggregate tests.
--
-- The `published` column defaults to FALSE so rows inserted without
-- explicitly setting it participate naturally in the bool aggregate tests.
--
-- Uses `IF NOT EXISTS` on the table DDL and `ADD COLUMN IF NOT EXISTS` on
-- the ALTER so the migration is idempotent — re-running it in a test session
-- that already applied the Task 5 posts migration does not raise an error.

ALTER TABLE posts ADD COLUMN IF NOT EXISTS published BOOLEAN NOT NULL DEFAULT FALSE;
