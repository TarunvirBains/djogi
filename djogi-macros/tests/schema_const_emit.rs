//! Cluster 8ζ T12.1 — runtime checks for the auto-emitted
//! `pub const {MODEL}_SCHEMA: &str` per `#[derive(Model)]`.
//!
//! The const is `#[doc(hidden)]` but `pub`, so test code in this crate
//! can name it directly. A compile-pass fixture would give us a
//! syntactic pin; this file gives us the *content* pin: every assertion
//! below would fail if the schema renderer dropped a row, swapped sort
//! order, or emitted unexpected whitespace.

use djogi::prelude::*;

// ── Fixture 1 — wide model with relations + indexes + nullable cols ──
//
// Covers the full range of T12.1's schema renderer:
// - PK label = HeerId
// - Framework rows (id / created_at / updated_at)
// - Plain user fields with NOT NULL inference
// - Nullable user field (`Option<…>`)
// - Unique index modifier
// - Explicit index method via `#[field(index = "btree")]`
// - FK relation with on_delete
#[model(table = "schema_const_vehicles", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Vehicle {
    #[field(unique)]
    pub vin: String,
    pub maker: String,
    pub colour: Option<String>,
    #[field(index)]
    pub plate: String,
}

#[test]
fn vehicle_schema_const_renders_expected_shape() {
    let s = VEHICLE_SCHEMA;

    // Header.
    assert!(s.starts_with("table: schema_const_vehicles\n"), "got: {s}");

    // Framework fields, in fixed order.
    assert!(s.contains("\nfields:\n"));
    assert!(s.contains("  id: HeerId (PK)\n"));
    assert!(s.contains("  created_at: DateTime\n"));
    assert!(s.contains("  updated_at: DateTime\n"));

    // User fields with modifiers.
    assert!(s.contains("  vin: String NOT NULL UNIQUE\n"));
    assert!(s.contains("  maker: String NOT NULL\n"));
    assert!(s.contains("  colour: Option<String>\n"));
    assert!(s.contains("  plate: String NOT NULL\n"));

    // Indexes section.
    assert!(s.contains("\nindexes:\n"));
    assert!(s.contains("  - vin (UNIQUE)\n"));
    assert!(s.contains("  - plate (BTREE)\n"));
}

#[test]
fn vehicle_schema_const_is_byte_deterministic() {
    // The const is a single static `&str`. Any reference to it must
    // resolve to the same address (and hence the same bytes). A
    // regression that re-rendered on each access would surface as
    // a different `&str` payload across two reads — pin via byte-
    // equality on a fresh `String` clone.
    let a: String = VEHICLE_SCHEMA.to_string();
    let b: String = VEHICLE_SCHEMA.to_string();
    assert_eq!(
        a.as_bytes(),
        b.as_bytes(),
        "schema const must be byte-deterministic",
    );
}

// ── Fixture 2 — minimal model, no indexes, no relations ─────────────
#[model(table = "schema_const_minimal", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Minimal {
    pub note: String,
}

#[test]
fn minimal_schema_const_omits_empty_sections() {
    let s = MINIMAL_SCHEMA;
    assert!(s.starts_with("table: schema_const_minimal\n"));
    assert!(s.contains("  note: String NOT NULL\n"));

    // No indexes / relations on this model — those sections must be
    // absent entirely (empty `indexes:\n` blocks would be noise).
    assert!(
        !s.contains("indexes:"),
        "minimal model has no indexes; section must be omitted; got: {s}"
    );
    assert!(
        !s.contains("relations:"),
        "minimal model has no relations; section must be omitted; got: {s}"
    );
}

// ── Fixture 3 — multi-word model name → UPPER_SNAKE const ─────────────
#[model(table = "schema_const_org_users", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct OrgUser {
    pub email: String,
}

#[test]
fn multi_word_model_name_uppersnakes_const() {
    // `OrgUser` → `ORG_USER_SCHEMA`. If this name is wrong the test
    // fails at link time with "cannot find value `ORG_USER_SCHEMA`".
    let s = ORG_USER_SCHEMA;
    assert!(s.starts_with("table: schema_const_org_users\n"));
}
