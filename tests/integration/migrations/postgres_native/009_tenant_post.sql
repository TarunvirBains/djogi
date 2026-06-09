-- tenant_post table with RLS policy for tenant isolation.
--
-- This migration is applied by hand in the integration test because the
-- DDL emission pipeline has not landed yet. The policy is
-- hand-applied here to exercise the runtime isolation contract.
--
-- The `true` second argument to `current_setting` makes missing-variable
-- tolerant: Postgres returns NULL instead of raising when the GUC is not set.
-- This is critical for test-setup ordering — a connection that has not yet
-- called SET LOCAL app.tenant_id sees NULL, which never equals any org_id
-- BIGINT value, so the row is hidden rather than the query erroring out.
--
-- RLS is enabled in PERMISSIVE mode (the default); no BY-PASS policy is
-- emitted here because the test user is not a superuser.

CREATE TABLE IF NOT EXISTS tenant_post (
    id          BIGINT PRIMARY KEY DEFAULT generate_id(),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    org_id      BIGINT NOT NULL,
    title       TEXT NOT NULL
);

ALTER TABLE tenant_post ENABLE ROW LEVEL SECURITY;

-- FORCE is not set so the table owner can still bypass RLS when needed.
CREATE POLICY tenant_post_tenant_isolation ON tenant_post
    USING (org_id = current_setting('app.tenant_id', true)::bigint);
