--  integration fixture: `persons` — one side of the
-- many-to-many pair.
--
-- The M2M suite exercises the explicit-through-model design: `Person`
-- and `Group` sit unchanged as ordinary models; the junction
-- `person_groups` (`007_person_groups.sql`) carries both FKs plus
-- relationship-specific columns (`role`). Any extra data about the
-- association lives on the through model, so `persons` itself only
-- needs the standard framework columns plus a name.
--
-- Scoping note: same test-schema scoping as `001_owners.sql` /
-- `002_fuel_types.sql` / `003_vehicles.sql` — issued via the shared
-- `setup_` helper, not via `sqlx::test(migrations = "...")`.

CREATE TABLE IF NOT EXISTS persons (
    id BIGINT PRIMARY KEY DEFAULT generate_id(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    name TEXT NOT NULL
);
