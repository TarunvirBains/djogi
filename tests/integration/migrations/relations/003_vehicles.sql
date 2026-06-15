-- integration fixture: `vehicles` table with two FK columns.
--
-- This is the anchor table for the `.fetch()` / `.resolved()`
-- integration tests. Two FK shapes are deliberately exercised:
--
--  - `owner_id`   NOT NULL  -> owners(id)  ON DELETE CASCADE
--   Exercises the `ForeignKey<Owner>` straight-through path.
--  - `fuel_type_id` NULL    -> fuel_types(id) ON DELETE RESTRICT
--   Exercises the `Option<ForeignKey<FuelType>>` branch of both the
--   macro-generated `FromRow` and the sqlx Decode impls.
--
-- The two distinct `ON DELETE` actions (`CASCADE` vs `RESTRICT`) are
-- documentary for migration-emitter tests; doesn't
-- assert cascade behavior itself. `RESTRICT` on the nullable FK also
-- guards against a surprise cross-test cascade bleed if some future
-- test deletes a fuel-type row that a live vehicle still references.
--
-- Scoping note: same as `001_owners.sql` / `002_fuel_types.sql` — this
-- is *test* schema, issued via the shared `setup_` helper.

CREATE TABLE IF NOT EXISTS vehicles (
  id BIGINT PRIMARY KEY DEFAULT generate_id(),
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  make TEXT NOT NULL,
  owner_id BIGINT NOT NULL REFERENCES owners(id) ON DELETE CASCADE,
  fuel_type_id BIGINT REFERENCES fuel_types(id) ON DELETE RESTRICT
);
