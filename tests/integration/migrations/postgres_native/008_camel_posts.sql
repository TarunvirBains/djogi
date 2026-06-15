-- camel_posts table for container-level serde rename_all tests.
--
-- Uses a JSONB spec column whose Rust schema is CamelSpec
-- (engine_type: i32, weight_kg: f32) with #[serde(rename_all = "camelCase")].
-- The on-disk JSON keys are "engineType" and "weightKg".

CREATE TABLE IF NOT EXISTS camel_posts (
  id     BIGINT PRIMARY KEY DEFAULT generate_id(),
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  spec    JSONB NULL
);
