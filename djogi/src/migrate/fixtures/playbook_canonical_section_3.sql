BEGIN;
    ALTER TABLE tbl DROP CONSTRAINT tbl_pkey;                               -- drop old PK
    ALTER TABLE tbl ADD CONSTRAINT tbl_pkey PRIMARY KEY USING INDEX idx_tbl_id_desc;
    ALTER TABLE tbl ALTER COLUMN id_desc SET DEFAULT heerid_next_desc();
    ALTER TABLE tbl ALTER COLUMN id DROP DEFAULT;                           -- old default off
    ALTER TABLE tbl DROP COLUMN id;                                         -- old column gone
    DROP TRIGGER zzz_tbl_autofill_desc ON tbl;                              -- trigger gone
    DROP FUNCTION zzz_tbl_autofill_desc() CASCADE;                          -- its function gone
    ALTER TABLE tbl RENAME COLUMN id_desc TO id;                            -- final rename
COMMIT;
