// Phase 8.5 Class B fix — `unique(...)` with index-only modifiers escalates
// to `UniqueIndex` (CREATE UNIQUE INDEX form) rather than staying as
// `UniqueConstraint` (ALTER TABLE ADD CONSTRAINT form). The modifiers below
// are only valid in the `CREATE INDEX … USING … (col modifier)` index-element
// grammar; Postgres rejects them in the table-constraint `UNIQUE (col_list)`
// column list.
//
// All forms below must compile without error. The macro silently escalates
// the kind from `UniqueConstraint` to `UniqueIndex`; no user-visible error
// is produced. The resulting descriptor carries `IndexKind::UniqueIndex`
// and a `..._uidx` name — verified by the unit tests in
// `djogi-macros/src/model/indexes.rs`.
//
// Non-btree `using = "<method>"` on `unique(...)` is NOT covered here —
// PostgreSQL unique indexes are btree-only, so the macro rejects every
// non-btree unique declaration at validation time. The five non-btree
// rejection cases (`gin` / `gist` / `brin` / `spgist` / `hash`) live in
// `compile_fail/phase85_model_indexes_<method>_unique.rs` (Phase 8.5 #83
// fix). Btree-compatible escalations — predicate, include,
// `nulls_not_distinct`, expression target, `concurrently`,
// opclass / DESC / NULLS modifiers — stay covered here and in the
// sibling `unique_concurrent` / `unique_partial` / `unique_with_modifiers`
// fixtures.
use djogi::prelude::*;

/// Top-level `opclass = "text_pattern_ops"` on `unique(...)`.
#[model(table = "uc_top_opclass", indexes(
    unique(fields = [email], opclass = "text_pattern_ops"),
))]
#[derive(Debug, Clone)]
pub struct TopOpclass {
    pub email: String,
}

/// Per-column `opclass` via the record form.
#[model(table = "uc_col_opclass", indexes(
    unique(fields = [(col = email, opclass = "text_pattern_ops")]),
))]
#[derive(Debug, Clone)]
pub struct ColOpclass {
    pub email: String,
}

/// Per-column `order = desc`.
#[model(table = "uc_col_desc", no_default, indexes(
    unique(fields = [(col = happened_at, order = desc)]),
))]
#[derive(Debug, Clone)]
pub struct ColDesc {
    pub happened_at: DateTime,
}

/// Per-column `nulls = first`.
#[model(table = "uc_col_nulls_first", indexes(
    unique(fields = [(col = slug, nulls = first)]),
))]
#[derive(Debug, Clone)]
pub struct ColNullsFirst {
    pub slug: Option<String>,
}

/// Per-column `nulls = last`.
#[model(table = "uc_col_nulls_last", indexes(
    unique(fields = [(col = slug, nulls = last)]),
))]
#[derive(Debug, Clone)]
pub struct ColNullsLast {
    pub slug: Option<String>,
}

/// Composite: opclass on one column and plain on another — escalates because
/// the first column has an index-only modifier.
#[model(table = "uc_composite_opclass", no_default, indexes(
    unique(fields = [(col = tenant_id, opclass = "int8_ops"), external_id]),
))]
#[derive(Debug, Clone)]
pub struct CompositeOpclass {
    pub tenant_id: HeerId,
    pub external_id: String,
}

fn main() {}
