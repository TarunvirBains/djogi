//! Descriptor-shape tests — composition-derive
//! provenance.
//!
//! Pins the new `FieldDescriptor::composed_via: Option<&'static str>`
//! slot under three conditions:
//!
//! 1. A `#[model(auditable)]` model carries
//!    `composed_via: Some("Auditable")` on its `created_by` column.
//! 2. A `#[model(soft_deletable)]` model carries
//!    `composed_via: Some("SoftDeletable")` on its `deleted_at` column.
//! 3. A regular user-declared field (no composition opt-in contributing
//!    it) keeps `composed_via: None`.
//!
//! These are pure descriptor-inspection tests — no Postgres, no async
//! runtime. They run as plain `#[test]` because the macro-emitted
//! `inventory::submit!` block populates `Model::descriptor()` at module
//! load time, before the test body runs.
//!
//! # Why provenance is metadata-only
//!
//! The migration differ does **not** key off
//! `composed_via` — a column flagged `Some("Auditable")` compares
//! identically to a hand-declared `created_by: Option<String>`. The
//! field is consumed by `djogi docs` and admin-UI surfaces
//! that want to distinguish framework-contributed columns from
//! adopter-authored ones, not by the schema-derivation pipeline.
//!
//! The companion file `compose_migration.rs` proves the
//! identity-of-emission claim end-to-end by lowering an
//! `Auditable + SoftDeletable` model to `CREATE TABLE` SQL and
//! asserting both composed columns appear with the same shape they
//! would carry if hand-declared.
//!
//! # `_insecurely` deferral
//!
//! Spec calls out a fourth test
//! (`_insecurely_warn_track_caller_source`) for the visage-bypass
//! audit warn. That helper itself is deferred — the
//! test cannot exist until the helper does.

use djogi::prelude::*;

// ---------------------------------------------------------------------------
// Test 1 — `#[model(auditable)]` tags `created_by` with
// `composed_via: Some("Auditable")`.
//
// The provenance slot is populated unconditionally by the model
// macro when it sees the `auditable` flag on the same model that
// declares the `created_by` column. Tracking provenance through
// `model_attrs.auditable` (instead of the column type) keeps the
// tag accurate even if a future amendment changes the column's
// stored type or default expression.
// ---------------------------------------------------------------------------

#[model(table = "audit_provenance", auditable)]
#[derive(Debug, Clone)]
pub struct AuditProvenance {
    pub note: String,
    pub created_by: Option<String>,
}

#[test]
fn auditable_field_descriptor_carries_composed_via() {
    let desc = AuditProvenance::descriptor();
    let created_by = desc
        .fields
        .iter()
        .find(|f| f.name == "created_by")
        .expect("AuditProvenance must declare a `created_by` field");
    assert_eq!(
        created_by.composed_via,
        Some("Auditable"),
        "#[model(auditable)] must tag the `created_by` column with \
         composed_via = Some(\"Auditable\") so docs / admin tooling \
         can render the column's provenance",
    );
}

// ---------------------------------------------------------------------------
// Test 2 — `#[model(soft_deletable)]` tags `deleted_at` with
// `composed_via: Some("SoftDeletable")`.
//
// Detection was tightened from field-name-only to
// field-name-plus-flag, eliminating the false-positive risk: 
// an adopter who declares a
// `deleted_at` column without opting into the composition no longer
// sees the (informational) tag on that column. Counter-test 3a below
// pins that tightening.
// ---------------------------------------------------------------------------

#[model(table = "soft_provenance", soft_deletable)]
#[derive(Debug, Clone)]
pub struct SoftProvenance {
    pub note: String,
    pub deleted_at: Option<djogi::DateTime>,
}

#[test]
fn soft_deletable_field_descriptor_carries_composed_via() {
    let desc = SoftProvenance::descriptor();
    let deleted_at = desc
        .fields
        .iter()
        .find(|f| f.name == "deleted_at")
        .expect("SoftProvenance must declare a `deleted_at` field");
    assert_eq!(
        deleted_at.composed_via,
        Some("SoftDeletable"),
        "a model carrying a `deleted_at` field must tag that column \
         with composed_via = Some(\"SoftDeletable\") so docs / admin \
         tooling can render the column's provenance",
    );
}

// ---------------------------------------------------------------------------
// Test 3 — Regular user-declared fields keep `composed_via: None`.
//
// Counter-test for the previous two cases. The `note` column on a
// model that opts into neither `Auditable` nor `SoftDeletable` (and
// declares neither `created_by` nor `deleted_at`) must stay at the
// constructor default of `None`. This pins the negative half of the
// contract so a future emitter that accidentally tags every column
// blows up loudly.
// ---------------------------------------------------------------------------

#[model(table = "regular_provenance")]
#[derive(Debug, Clone)]
pub struct RegularProvenance {
    pub value: String,
}

#[test]
fn regular_field_composed_via_none() {
    let desc = RegularProvenance::descriptor();
    let value = desc
        .fields
        .iter()
        .find(|f| f.name == "value")
        .expect("RegularProvenance must declare a `value` field");
    assert!(
        value.composed_via.is_none(),
        "a user-declared field on a model with no composition opt-in \
         must keep composed_via = None; got {:?}",
        value.composed_via,
    );
}

// ---------------------------------------------------------------------------
// Test 3a — Tightening counter-test.
//
// A model that declares a `deleted_at: Option<DateTime>` field
// WITHOUT `#[model(soft_deletable)]` must NOT carry the
// `composed_via: Some("SoftDeletable")` tag. Original detection
// was field-name-only; it was tightened to field-name-plus-flag,
// eliminating the false-positive risk. This test pins the tightening
// so a regression that drops the flag check fires loudly.
// ---------------------------------------------------------------------------

#[model(table = "deleted_at_no_optin")]
#[derive(Debug, Clone)]
pub struct DeletedAtNoOptin {
    pub note: String,
    pub deleted_at: Option<djogi::DateTime>,
}

#[test]
fn deleted_at_without_opt_in_keeps_composed_via_none() {
    let desc = DeletedAtNoOptin::descriptor();
    let deleted_at = desc
        .fields
        .iter()
        .find(|f| f.name == "deleted_at")
        .expect("DeletedAtNoOptin must declare a `deleted_at` field");
    assert!(
        deleted_at.composed_via.is_none(),
        "a `deleted_at` column on a model that does NOT opt into \
         #[model(soft_deletable)] must keep composed_via = None; \
         detection was tightened from field-name-only to \
         field-name-plus-flag. got {:?}",
        deleted_at.composed_via,
    );
}

// ---------------------------------------------------------------------------
// Test 4 — Framework columns (`id`, `created_at`, `updated_at`) keep
// `composed_via: None`.
//
// Implicit framework injection is not a composition derive — the
// columns are part of every model's identity contract, not opt-in
// behaviour layered on top. The descriptor must reflect that
// distinction so consumers walking `descriptor.fields` see provenance
// only on the columns that actually came from a derive.
// ---------------------------------------------------------------------------

#[test]
fn framework_fields_composed_via_none() {
    let desc = AuditProvenance::descriptor();
    for col in ["id", "created_at", "updated_at"] {
        let f = desc
            .fields
            .iter()
            .find(|f| f.name == col)
            .unwrap_or_else(|| panic!("AuditProvenance must declare framework column `{col}`"));
        assert!(
            f.composed_via.is_none(),
            "framework column `{col}` must keep composed_via = None; \
             framework injection is not a composition derive. got {:?}",
            f.composed_via,
        );
    }
}
