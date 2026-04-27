BEGIN;
    -- 1. Drop every child's old FK.
    ALTER TABLE c DROP CONSTRAINT c_p_id_fkey;          -- repeat per child

    -- 2. Promote the parent (same body as §3.6).
    ALTER TABLE parent DROP CONSTRAINT parent_pkey;
    ALTER TABLE parent ADD CONSTRAINT parent_pkey PRIMARY KEY USING INDEX idx_parent_id_desc;
    ALTER TABLE parent ALTER COLUMN id_desc SET DEFAULT heerid_next_desc();
    ALTER TABLE parent ALTER COLUMN id DROP DEFAULT;
    ALTER TABLE parent DROP COLUMN id;
    DROP TRIGGER zzz_parent_autofill_desc ON parent;
    DROP FUNCTION zzz_parent_autofill_desc() CASCADE;
    ALTER TABLE parent RENAME COLUMN id_desc TO id;

    -- 3. Finalise every child.
    ALTER TABLE c DROP COLUMN p_id;
    DROP TRIGGER zzz_c_autofill_desc ON c;
    DROP FUNCTION zzz_c_autofill_desc() CASCADE;
    ALTER TABLE c RENAME COLUMN p_id_desc TO p_id;
    ALTER TABLE c
      ADD CONSTRAINT c_p_id_fkey
      FOREIGN KEY (p_id) REFERENCES parent(id);
COMMIT;
