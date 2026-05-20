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

/// Non-btree `using` on `unique(...)` — escalates because `ADD CONSTRAINT UNIQUE`
/// has no `USING` clause; preserving the method requires `CREATE UNIQUE INDEX`.
#[model(table = "uc_gist_using", no_default, indexes(
    unique(fields = [location], using = "gist"),
))]
#[derive(Debug, Clone)]
pub struct GistUsing {
    pub location: String,
}

fn main() {}
