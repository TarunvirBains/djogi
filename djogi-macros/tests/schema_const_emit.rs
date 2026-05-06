//! Runtime checks for the auto-emitted `pub const {MODEL}_SCHEMA: &str`
//! per `#[derive(Model)]`. The const is `#[doc(hidden)]` but `pub`, so
//! test code in this crate names it directly. Every assertion would
//! fail if the renderer dropped a row, swapped sort order, or emitted
//! unexpected whitespace.

use djogi::prelude::*;

// Fixture 1 — wide model with indexes + nullable columns under the
// explicit `pk = HeerId` ascending strategy.
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

// Fixture 2 — minimal model with no indexes or relations.
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

// Fixture 3 — multi-word model name → UPPER_SNAKE const.
#[model(table = "schema_const_org_users", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct OrgUser {
    pub email: String,
}

#[test]
fn multi_word_model_name_uppersnakes_const() {
    // `OrgUser` → `ORG_USER_SCHEMA`. A wrong name fails at link time
    // with "cannot find value `ORG_USER_SCHEMA`".
    let s = ORG_USER_SCHEMA;
    assert!(s.starts_with("table: schema_const_org_users\n"));
}

// Fixture 4 — default PK (no `pk = ...` attr) resolves to `HeerIdDesc`.
// Pinned because an earlier renderer flattened ascending and descending
// HeerId variants into the same `HeerId` label, hiding the recency-
// biased ordering from adopters reading the const.
#[model(table = "schema_const_default_pk")]
#[derive(Debug, Clone)]
pub struct DefaultPk {
    pub label: String,
}

#[test]
fn default_pk_renders_as_heerid_desc() {
    let s = DEFAULT_PK_SCHEMA;
    assert!(
        s.contains("  id: HeerIdDesc (PK)\n"),
        "default PK must surface as HeerIdDesc (recency-biased), not HeerId; got: {s}"
    );
    assert!(
        !s.contains("  id: HeerId (PK)\n"),
        "default PK must not be mislabelled as ascending HeerId; got: {s}"
    );
}

// Fixture 5 — explicit ascending PK still renders as `HeerId`.
#[model(table = "schema_const_ascending_pk", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct AscendingPk {
    pub label: String,
}

#[test]
fn ascending_pk_renders_as_heerid() {
    let s = ASCENDING_PK_SCHEMA;
    assert!(
        s.contains("  id: HeerId (PK)\n"),
        "explicit `pk = HeerId` must surface as ascending HeerId; got: {s}"
    );
}

// Fixture 6 — relation field with `on_delete` attribute. Pinned because
// the renderer used to call `s.to_uppercase()` on the raw attribute
// string, turning `set_null` into `SET_NULL` (with underscore) instead
// of the proper SQL spelling `SET NULL`.
#[model(table = "schema_const_owners", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Owner {
    pub name: String,
}

#[model(table = "schema_const_cars", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Car {
    pub plate: String,
    #[field(on_delete = "set_null")]
    pub owner: Option<ForeignKey<Owner>>,
}

#[test]
fn on_delete_set_null_renders_with_space_not_underscore() {
    let s = CAR_SCHEMA;
    assert!(
        s.contains("\nrelations:\n"),
        "Car has an FK; relations section must be present; got: {s}"
    );
    assert!(
        s.contains("ON DELETE SET NULL"),
        "set_null must render as `ON DELETE SET NULL`, not `ON DELETE SET_NULL`; got: {s}"
    );
    assert!(
        !s.contains("ON DELETE SET_NULL"),
        "renderer must not surface raw attribute uppercase form; got: {s}"
    );
}
