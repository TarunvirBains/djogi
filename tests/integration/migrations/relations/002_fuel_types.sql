-- integration fixture: minimal `fuel_types` lookup table.
--
-- Partner to `003_vehicles.sql`'s nullable FK — gives a target
-- table that exercises the `Option<ForeignKey<T>>` branch of the
-- `FromRow` decode and the `.fetch()` round-trip. The suffix
-- keeps this namespaced away from fixtures so all
-- tests can share a database without DDL collisions.
--
-- Scoping note: this file ships here (rather than in the
-- framework-level `migrations/` submodule) because it is *test*
-- schema, not application schema. machinery will be introduced; 
-- for now, integration tests issue this DDL
-- manually via the shared `setup_test_schema` helper.
--
-- `generate_id()` is provided by `heeranjid_sqlx::install_schema`,
-- which test setup calls before applying this file, matching
-- the pattern `model.rs` and `queryset.rs` already use.

CREATE TABLE IF NOT EXISTS fuel_types (
  id BIGINT PRIMARY KEY DEFAULT generate_id(),
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  name TEXT NOT NULL
);
