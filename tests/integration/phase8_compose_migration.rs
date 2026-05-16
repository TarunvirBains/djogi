//! Phase 8α T2.5 migration round-trip — composed columns lower to
//! the same `CREATE TABLE` SQL as hand-declared columns.
//!
//! What this file pins:
//!
//! Spec line 1101 — generate the `CREATE TABLE` for an
//! `Auditable + SoftDeletable` model and assert the SQL contains both
//! `created_by TEXT` (nullable) and `deleted_at TIMESTAMPTZ` (nullable),
//! matching v3 §T2 line 231's acceptance criterion.
//!
//! Spec line 1124 says the migration differ does NOT key off
//! `composed_via`. This test proves it: the round-trip path runs from
//! `Model::descriptor()` → `project_from_inventory` →
//! `diff_bucket_maps` against an empty before-state →
//! `lower_delta` → SQL string. If migration emission ever started
//! depending on `composed_via`, this test would either start failing
//! (provenance leaked into the SQL) or produce SQL different from a
//! hand-declared baseline (provenance gated emission). Either way it
//! breaks loudly.
//!
//! # Why an empty-before diff is enough
//!
//! `diff_bucket_maps(empty, projected)` produces an `AddTable`
//! `SchemaOperation` for every projected model. The `AddTable`
//! emitter is the path that produces `CREATE TABLE` SQL — exactly
//! the surface this test cares about. Adding a column changes a
//! different code path (`emit_add_column`) which is covered by other
//! tests in the migration suite.
//!
//! # Why `Auditable + SoftDeletable` together
//!
//! Spec line 1101 names the combination explicitly. Stacking both
//! composition derives on one model exercises:
//!
//! - The model macro's `auditable` flag flowing through to descriptor
//!   emission (`#[model(auditable)]` path).
//! - The `#[model(soft_deletable)]` opt-in (T2.6) emitting the trait
//!   impl alongside the auditable surface.
//! - The descriptor emitter tagging both `created_by` and `deleted_at`
//!   independently with the right provenance string.
//! - The migration emitter NOT discriminating between composed and
//!   hand-declared columns when it lowers either to SQL.

use std::collections::BTreeMap;

use djogi::migrate::diff::{Classification, SchemaDelta, SchemaOperation};
use djogi::migrate::projection::{BucketKey, project_from_inventory};
use djogi::migrate::sql::lower_delta;
use djogi::prelude::*;

// ---------------------------------------------------------------------------
// The model under test — both composition derives stacked on the
// same struct. Adopter migration story: this is exactly the shape a
// production model carrying both audit and soft-delete metadata
// would adopt.
// ---------------------------------------------------------------------------

#[model(table = "phase8_compose_round_trip", auditable, soft_deletable)]
#[derive(Debug, Clone)]
pub struct Phase8ComposeRoundTrip {
    pub note: String,
    pub created_by: Option<String>,
    pub deleted_at: Option<djogi::DateTime>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build an empty `AppliedSchema` keyed bucket-map. The diff against
/// the projected inventory is then "add every model from scratch" —
/// which produces one `AddTable` per registered model.
fn empty_before() -> BTreeMap<BucketKey, djogi::migrate::AppliedSchema> {
    BTreeMap::new()
}

/// Walk the deltas produced by `diff_bucket_maps(empty, projected)`
/// and locate the `AddTable` operation for `target_table`. Returns
/// the corresponding `SchemaDelta` containing only that operation —
/// keeping `lower_delta` focused on a single-table emission so
/// neighbouring inventory entries don't leak SQL into the assertion
/// surface.
fn extract_add_table_delta(deltas: Vec<SchemaDelta>, target_table: &str) -> SchemaDelta {
    for delta in deltas {
        for op in &delta.operations {
            if let SchemaOperation::AddTable(t) = op
                && t.table == target_table
            {
                return SchemaDelta {
                    bucket: delta.bucket.clone(),
                    operations: vec![op.clone()],
                    classification: Classification::Additive,
                };
            }
        }
    }
    panic!(
        "no AddTable operation found for `{target_table}` in the projected delta set; \
         the model macro emitted no descriptor or the projection lost the table",
    );
}

// ---------------------------------------------------------------------------
// Test 1 — round-trip emission (the spec line 1101 anchor).
// ---------------------------------------------------------------------------

#[test]
fn migration_emits_composed_columns_identically() {
    let projected = project_from_inventory().expect("project from inventory");
    let deltas = djogi::migrate::diff::diff_bucket_maps(&empty_before(), &projected)
        .expect("diff_bucket_maps against empty before-state");

    let delta = extract_add_table_delta(deltas, "phase8_compose_round_trip");
    let ops = lower_delta(&delta).expect("lower the AddTable delta to SQL");
    assert_eq!(
        ops.len(),
        1,
        "single-operation delta should lower to a single OperationSql"
    );
    let sql = &ops[0].up;

    // Spec line 1101 acceptance criterion (v3 §T2 line 231):
    // `created_by TEXT NULL` and `deleted_at TIMESTAMPTZ NULL`. The
    // current emitter renders nullability by absence of `NOT NULL`
    // rather than an explicit `NULL` keyword (Postgres-compatible
    // since `NULL` is the column default), so we assert the column
    // tokens directly without the trailing `NULL` — and assert the
    // absence of `NOT NULL` to lock in the nullable shape.
    assert!(
        sql.contains("\"created_by\" TEXT"),
        "AddTable SQL must contain the composed `created_by TEXT` column;\nSQL was:\n{sql}",
    );
    assert!(
        sql.contains("\"deleted_at\" TIMESTAMPTZ"),
        "AddTable SQL must contain the composed `deleted_at TIMESTAMPTZ` column;\nSQL was:\n{sql}",
    );

    // Both composed columns must be nullable — `Option<String>` /
    // `Option<DateTime>` on the struct lowers to NO `NOT NULL`
    // suffix on the column. If the emitter started forcing NOT NULL
    // on composed columns, soft-deletion semantics would break
    // (a NOT NULL `deleted_at` makes "live row" unrepresentable).
    let created_by_idx = sql
        .find("\"created_by\"")
        .expect("created_by must appear in CREATE TABLE SQL");
    let after_created_by = &sql[created_by_idx..];
    let next_comma = after_created_by
        .find(',')
        .or_else(|| after_created_by.find(')'))
        .unwrap_or(after_created_by.len());
    let created_by_def = &after_created_by[..next_comma];
    assert!(
        !created_by_def.contains("NOT NULL"),
        "created_by must remain nullable; got column definition `{created_by_def}`",
    );

    let deleted_at_idx = sql
        .find("\"deleted_at\"")
        .expect("deleted_at must appear in CREATE TABLE SQL");
    let after_deleted_at = &sql[deleted_at_idx..];
    let next_comma = after_deleted_at
        .find(',')
        .or_else(|| after_deleted_at.find(')'))
        .unwrap_or(after_deleted_at.len());
    let deleted_at_def = &after_deleted_at[..next_comma];
    assert!(
        !deleted_at_def.contains("NOT NULL"),
        "deleted_at must remain nullable; got column definition `{deleted_at_def}`",
    );

    // Sanity: the SQL is in fact a CREATE TABLE statement for our
    // table. Cheap to assert and gives a clearer failure message
    // than a downstream column-presence check if the projection
    // mis-routes the model.
    assert!(
        sql.starts_with("CREATE TABLE \"phase8_compose_round_trip\""),
        "AddTable up SQL should begin with `CREATE TABLE \"phase8_compose_round_trip\"`;\nSQL was:\n{sql}",
    );
}

// ---------------------------------------------------------------------------
// Test 2 — composed columns lower to the same SQL fragment a hand-
// declared baseline would.
//
// Builds a parallel "hand-declared" model with identical columns
// (no composition derives) and verifies the `created_by` / `deleted_at`
// substrings appear identically in both emitted CREATE TABLE
// statements. If migration emission ever started keying off
// `composed_via`, the two strings would diverge.
// ---------------------------------------------------------------------------

#[model(table = "phase8_compose_round_trip_baseline")]
#[derive(Debug, Clone)]
pub struct Phase8ComposeRoundTripBaseline {
    pub note: String,
    pub created_by: Option<String>,
    pub deleted_at: Option<djogi::DateTime>,
}

#[test]
fn composed_columns_match_hand_declared_baseline() {
    let projected = project_from_inventory().expect("project from inventory");
    let deltas = djogi::migrate::diff::diff_bucket_maps(&empty_before(), &projected)
        .expect("diff_bucket_maps against empty before-state");

    let composed_delta = extract_add_table_delta(deltas.clone(), "phase8_compose_round_trip");
    let baseline_delta = extract_add_table_delta(deltas, "phase8_compose_round_trip_baseline");

    let composed_sql = lower_delta(&composed_delta)
        .expect("lower composed")
        .remove(0)
        .up;
    let baseline_sql = lower_delta(&baseline_delta)
        .expect("lower baseline")
        .remove(0)
        .up;

    // Strip the table-name preamble (`CREATE TABLE "<name>" (`) and
    // the trailing PK / closing paren, then compare the column-
    // definition body. Normalize generated constraint names because
    // CHECK names deliberately include the table name; the invariant
    // here is that composed-vs-hand-declared field emission is otherwise
    // identical.
    fn normalized_column_body(sql: &str, table_name: &str) -> String {
        let open = sql
            .find('(')
            .expect("CREATE TABLE SQL must contain an opening paren");
        let close = sql
            .rfind(')')
            .expect("CREATE TABLE SQL must contain a closing paren");
        sql[open + 1..close].replace(table_name, "$TABLE")
    }
    assert_eq!(
        normalized_column_body(&composed_sql, "phase8_compose_round_trip"),
        normalized_column_body(&baseline_sql, "phase8_compose_round_trip_baseline"),
        "composed `Auditable + SoftDeletable` model must emit the same column body \
         as a hand-declared model with the same fields; emission must NOT depend \
         on `composed_via`. composed:\n{composed_sql}\n\nbaseline:\n{baseline_sql}",
    );
}
