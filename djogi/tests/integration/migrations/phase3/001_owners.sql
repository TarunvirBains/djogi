-- Phase 3 integration fixture: minimal `owners_p3` table.
--
-- Consumed by the Phase 3 Task 3 integration tests (`.fetch()` /
-- `.resolved()` against live Postgres). The `_p3` suffix keeps this
-- table namespaced away from the Phase 1/2 integration fixtures so
-- all three phases' tests can share a database without DDL
-- collisions.
--
-- Scoping note: this file ships here (rather than in the
-- framework-level `migrations/` submodule) because it is *test*
-- schema, not application schema. Phase 6 will introduce the real
-- migration machinery; for now, integration tests issue this DDL
-- manually during test setup.
--
-- `generate_id()` is provided by `heeranjid_sqlx::install_schema`,
-- which Phase 3 test setup calls before applying this file, matching
-- the pattern `phase1_model.rs` and `phase2_queryset.rs` already use.

CREATE TABLE IF NOT EXISTS owners_p3 (
    id BIGINT PRIMARY KEY DEFAULT generate_id(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    name TEXT NOT NULL
);
