-- vehicles_deep table for typed JSONB deep-path filter tests.
--
-- The `specs` column holds a JSONB object that maps to `VehicleSpecs` /
-- `VehicleDeepSpecs` (containing a nested `EngineSpecs` struct). Tests exercise
-- filtering at depth 2 (specs -> engine -> cylinders) and depth 3
-- (specs -> engine -> performance -> horsepower).

CREATE TABLE IF NOT EXISTS vehicles_deep (
  id     BIGINT PRIMARY KEY DEFAULT generate_id(),
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  name    TEXT NOT NULL,
  specs    JSONB NULL
);
