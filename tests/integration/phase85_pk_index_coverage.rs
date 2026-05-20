// Phase 8.5 issue #83 — macro-path integration coverage for PK
// no-id-index and field-level index synthesis.
//
// These two tests close the gap identified in the strict-swe BLOCK-1
// sweep: the projection-layer unit test in `projection.rs` builds
// `FieldDescriptor` manually and therefore cannot detect drift in
// `framework_field_descriptor` itself (e.g., a future regression that
// re-introduces `indexed: true` in any of the five PK strategy arms).
//
// By attaching real `#[derive(Model)]` structs to `#[djogi_test(sync_models
// = [...])]` fixtures, we exercise the full pipeline:
//
//   proc-macro emission → inventory registration → `project_from_inventory()`
//   → AppliedSchema index list → assertion
//
// The assertions are made via `project_from_inventory()` (the typed
// projection surface) rather than raw catalog queries, per the CLAUDE.md
// "tests must use djogi structs, not raw escape hatches" rule.
//
// Companion unit test: `projection.rs::tests::
//   framework_pk_does_not_synthesize_id_idx_on_fresh_addtable`
//   (projection-layer pin)
// Companion unit test: `projection.rs::tests::
//   field_indexed_true_synthesises_one_canonical_index_in_global`
//   (positive synthesis pin at the projection layer)

use djogi::migrate::projection::{BucketKey, project_from_inventory};
use djogi::prelude::*;

/// Returns the global `(main, "")` bucket key used for models that
/// carry no `#[model(app = ...)]` declaration.
fn global_key() -> BucketKey {
    BucketKey {
        database: "main".to_string(),
        app: String::new(),
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Models under test
// ───────────────────────────────────────────────────────────────────────────

/// Tiny `HeerId`-PK model with no `#[field(index)]` and no
/// `#[model(indexes(...))]`.  After the djogi#83 descriptor fix, the
/// macro emits `indexed: false` for the framework `id` field, so the
/// projection must produce zero synthetic indexes for this table.
#[model(table = "phase85_pk_no_id_tiny", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct PkNoIdTiny {
    /// Lone user field — satisfies the "at least one user field"
    /// model validity rule without introducing any indexed columns.
    pub name: String,
}

/// Tiny model with one `#[field(index)]` on a plain `String` field and
/// no explicit `#[model(indexes(...))]`.  The projection must synthesise
/// exactly one `IndexSchema` for this table, named
/// `phase85_field_idx_tiny_label_idx` (canonical `<table>_<col>_idx`).
#[model(table = "phase85_field_idx_tiny", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct FieldIdxTiny {
    /// The `#[field(index)]` attribute sets `indexed: true` in the
    /// emitted `FieldDescriptor`, which the projection converts into a
    /// synthetic BTree index named `phase85_field_idx_tiny_label_idx`.
    #[field(index)]
    pub label: String,
}

// ───────────────────────────────────────────────────────────────────────────
// Test 1 — framework PK must not produce a synthetic `_id_idx` entry
// ───────────────────────────────────────────────────────────────────────────

/// Macro-path BLOCK-1 regression guard (djogi#83): a real
/// `#[model(pk = HeerId)]` model with no `#[field(index)]` annotations
/// must produce zero index entries in `project_from_inventory()` for its
/// table.
///
/// This test detects drift in `framework_field_descriptor` at the macro
/// layer — if any PK strategy arm is changed to emit `indexed: true`,
/// the projection will synthesise a `phase85_pk_no_id_tiny_id_idx`
/// entry and this assertion will fire.  The companion projection unit
/// test (`framework_pk_does_not_synthesize_id_idx_on_fresh_addtable`)
/// pins the projection-layer behaviour against a manually constructed
/// descriptor and cannot detect macro-layer drift.
#[djogi::djogi_test(sync_models = [PkNoIdTiny])]
async fn pk_no_id_idx_macro_path(_ctx: djogi::DjogiContext) {
    // `project_from_inventory()` is synchronous — safe to call from
    // inside an async test body.  This binary's inventory contains only
    // `PkNoIdTiny` and `FieldIdxTiny` (defined in this file); no other
    // models are linked into this test binary.
    let projected = project_from_inventory()
        .expect("project_from_inventory must succeed for these simple models");
    let global = projected
        .get(&global_key())
        .expect("global bucket is always present in any projection result");

    // Filter to indexes owned by the PkNoIdTiny table.
    let table_indexes: Vec<&str> = global
        .indexes
        .iter()
        .filter(|i| i.table == "phase85_pk_no_id_tiny")
        .map(|i| i.name.as_str())
        .collect();

    assert!(
        table_indexes.is_empty(),
        "expected no synthetic indexes for `phase85_pk_no_id_tiny` \
         (framework PK carries indexed: false, user field `name` has no \
         #[field(index)]); got: {:?}",
        table_indexes,
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Test 2 — `#[field(index)]` on a plain field must synthesise one index
// ───────────────────────────────────────────────────────────────────────────

/// Macro-path field-level synthesis coverage (djogi#83 sweep CLASS B
/// adjacent): a real `#[field(index)]` annotation on a `String` field
/// must produce exactly one `IndexSchema` in `project_from_inventory()`,
/// named `phase85_field_idx_tiny_label_idx` (canonical
/// `<table>_<col>_idx`).
///
/// This exercises the end-to-end path: proc-macro sets `indexed: true`
/// in the emitted `FieldDescriptor` → `project_from_iters` field-level
/// fanout loop creates the synthetic `IndexSchema` → assertion confirms
/// the name and table.  The companion projection unit test
/// (`field_indexed_true_synthesises_one_canonical_index_in_global`) pins
/// the same synthesis at the projection layer against a manually
/// constructed descriptor.
#[djogi::djogi_test(sync_models = [FieldIdxTiny])]
async fn field_index_emitted_macro_path(_ctx: djogi::DjogiContext) {
    let projected = project_from_inventory()
        .expect("project_from_inventory must succeed for these simple models");
    let global = projected
        .get(&global_key())
        .expect("global bucket is always present in any projection result");

    // Collect only indexes for the FieldIdxTiny table.
    let table_indexes: Vec<(&str, &str)> = global
        .indexes
        .iter()
        .filter(|i| i.table == "phase85_field_idx_tiny")
        .map(|i| (i.table.as_str(), i.name.as_str()))
        .collect();

    assert_eq!(
        table_indexes.len(),
        1,
        "expected exactly one synthetic index for `phase85_field_idx_tiny` \
         (#[field(index)] on `label`); got: {:?}",
        table_indexes,
    );
    assert_eq!(
        table_indexes[0].1, "phase85_field_idx_tiny_label_idx",
        "synthetic field-level index must follow the <table>_<col>_idx \
         naming convention; got `{}`",
        table_indexes[0].1,
    );
}
