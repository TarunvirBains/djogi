-- Phase 3 integration fixture: minimal `fuel_types_p3` lookup table.
--
-- Partner to `003_vehicles.sql`'s nullable FK — gives Task 3 a target
-- table that exercises the `Option<ForeignKey<T>>` branch of the
-- `FromRow` decode and the `.fetch()` round-trip. The `_p3` suffix
-- keeps this namespaced away from Phase 1/2 fixtures so all phases'
-- tests can share a database without DDL collisions.
--
-- Scoping note: this file ships here (rather than in the
-- framework-level `migrations/` submodule) because it is *test*
-- schema, not application schema. Phase 6 will introduce the real
-- migration machinery; for now, integration tests issue this DDL
-- manually via the shared `setup_phase3` helper (see Q10 resolution
-- in the Phase 3 plan — the shared-helpers pattern was preferred over
-- `#[sqlx::test(migrations = ...)]`).
--
-- `generate_id()` is provided by `heeranjid_sqlx::install_schema`,
-- which Phase 3 test setup calls before applying this file, matching
-- the pattern `phase1_model.rs` and `phase2_queryset.rs` already use.

CREATE TABLE IF NOT EXISTS fuel_types_p3 (
    id BIGINT PRIMARY KEY DEFAULT generate_id(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    name TEXT NOT NULL
);
