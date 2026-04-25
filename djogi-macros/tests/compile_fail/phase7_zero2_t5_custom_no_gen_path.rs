// Phase 7-Zero-2 T5 — custom PK types that implement neither
// `PrimaryKeyDbGen` nor `PrimaryKeyClientGen` must not be bulk-insertable.
//
// `djogi::primary_key!` makes the `bulk_sql` (DB-gen) and `generate`
// (client-gen) entries optional. When both are absent the newtype still
// satisfies the `PrimaryKey` contract — it carries a sentinel, a sql
// type, and a default_sql — but neither generation path is wired. Post-
// T5 `#[model]` emits a `bulk_create` body that calls
// `<Self::Pk as PrimaryKeyDbGen>::generate_many`, so the absence of the
// impl surfaces as a compile error at model-definition time — the body
// type-checks even when `bulk_create` is never invoked.

use djogi::prelude::*;

djogi::primary_key! {
    pub struct Orphan(i64);
    sql_type = "BIGINT";
    default_sql = "nextval('orphan_seq')";
    // No `bulk_sql` -> no PrimaryKeyDbGen impl.
    // No `generate` -> no PrimaryKeyClientGen impl.
}

#[model(table = "orphans", pk = Orphan)]
#[derive(Debug, Clone)]
pub struct OrphanRow {
    pub name: String,
}

fn main() {}
