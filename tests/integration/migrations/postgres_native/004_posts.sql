-- posts table with array columns and JSONB column.
--
-- Columns:
-- - `title`       — plain TEXT, required.
-- - `tags`        — TEXT[] (array of strings). Used by array_contains,
--                   array_overlap, and array_len tests.
-- - `view_counts` — INTEGER[] (array of i32). Used by array_len_filters test.
-- - `specs`       — JSONB. Used by jsonb_flat_path_filter and
--                   jsonb_preserves_unknown_fields tests.
--
-- All three array/JSONB columns are nullable so tests can insert rows with
-- known values without always supplying all columns.

CREATE TABLE IF NOT EXISTS posts (
    id          BIGINT PRIMARY KEY DEFAULT generate_id(),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    title       TEXT NOT NULL,
    tags        TEXT[] NOT NULL DEFAULT '{}',
    view_counts INTEGER[] NOT NULL DEFAULT '{}',
    specs       JSONB NULL
);
