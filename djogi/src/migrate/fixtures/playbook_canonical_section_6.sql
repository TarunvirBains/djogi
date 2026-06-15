BEGIN;
 ALTER TABLE nodes DROP CONSTRAINT nodes_parent_id_fkey;
 ALTER TABLE nodes DROP CONSTRAINT nodes_pkey;
 ALTER TABLE nodes ADD CONSTRAINT nodes_pkey PRIMARY KEY USING INDEX idx_nodes_id_desc;
 ALTER TABLE nodes ALTER COLUMN id_desc SET DEFAULT heerid_next_desc();
 ALTER TABLE nodes ALTER COLUMN id DROP DEFAULT;
 ALTER TABLE nodes DROP COLUMN id;
 ALTER TABLE nodes DROP COLUMN parent_id;
 DROP TRIGGER zzz_nodes_autofill_desc ON nodes;
 DROP FUNCTION zzz_nodes_autofill_desc() CASCADE;
 ALTER TABLE nodes RENAME COLUMN id_desc  TO id;
 ALTER TABLE nodes RENAME COLUMN parent_id_desc TO parent_id;
 ALTER TABLE nodes
  ADD CONSTRAINT nodes_parent_id_fkey
  FOREIGN KEY (parent_id) REFERENCES nodes(id);
COMMIT;
