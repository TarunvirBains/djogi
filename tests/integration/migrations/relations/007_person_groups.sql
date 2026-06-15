-- integration fixture: `person_groups` — the through model
-- for the M2M pair of `persons` ↔ `groups`.
--
-- Djogi's M2M design is **explicit through models**: the junction is a
-- full `Model` (opts into `#[model(through)]`) rather than an implicit
-- association table the framework manages behind the scenes. This lets
-- the through row carry relationship-specific columns (`role`) and lets
-- users query it with the same `QuerySet` API as any other model.
--
-- Two NOT NULL FK columns (`person_id`, `group_id`) with
-- `ON DELETE CASCADE` — deleting either side tears down the associated
-- junction rows, which matches the expected M2M semantics and keeps
-- test teardown simple. The composite `UNIQUE (person_id, group_id)`
-- constraint pins the invariant that any given (person, group) pair
-- appears at most once; attempting to `add_related()` the same pair
-- twice surfaces as a Postgres uniqueness violation rather than a
-- silent duplicate.
--
-- Scoping note: same test-schema scoping as `001_owners.sql` through
-- `006_groups.sql` — issued via the shared `setup_` helper.

CREATE TABLE IF NOT EXISTS person_groups (
  id BIGINT PRIMARY KEY DEFAULT generate_id(),
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  person_id BIGINT NOT NULL REFERENCES persons(id) ON DELETE CASCADE,
  group_id BIGINT NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
  role   TEXT NOT NULL,
  UNIQUE (person_id, group_id)
);
