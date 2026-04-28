   BEGIN;
       -- Drop the old PK on the partitioned parent (propagates to partitions).
       ALTER TABLE parent DROP CONSTRAINT parent_pkey;

       -- Create the replacement PK in the canonical partitioned-table shape.
       -- The existing UNIQUE index from step 5 can reduce risk, but Postgres
       -- may still scan partitions or build replacement indexes here.
       ALTER TABLE parent ADD PRIMARY KEY (partition_key, id_desc);

       -- The remainder of the cutover (alter default, drop old column,
       -- drop trigger, rename) proceeds as in §7.1's single-table cutover,
       -- but applied to the parent and propagating to partitions.
       ALTER TABLE parent ALTER COLUMN id_desc SET DEFAULT heerid_next_desc();
       ALTER TABLE parent ALTER COLUMN id DROP DEFAULT;
       ALTER TABLE parent DROP COLUMN id;
       DROP TRIGGER zzz_parent_autofill_desc ON parent;
       DROP FUNCTION zzz_parent_autofill_desc() CASCADE;
       ALTER TABLE parent RENAME COLUMN id_desc TO id;
   COMMIT;
