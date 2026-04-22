-- Phase 5 Task 5 fixup: typed_posts table for Jsonb<T> round-trip tests.
--
-- Uses the same schema as `posts` but the Rust model declares `specs` as
-- `Jsonb<PostSpec>` (typed schema) rather than `Jsonb<serde_json::Value>`.
-- Keeping them separate avoids collisions with the existing array/JSONB tests.

CREATE TABLE IF NOT EXISTS typed_posts (
    id          BIGINT PRIMARY KEY DEFAULT generate_id(),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    title       TEXT NOT NULL,
    specs       JSONB NULL
);
