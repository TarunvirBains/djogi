//! visage-derived fields, Tier 1.
//!
//! Exercises the `#[derived(name, ty, scopes, sql, rust, doc)]`
//! attribute end to end:
//!
//! - Struct-level attribute parses and the derived field appears on
//!   every scoped visage.
//! - The `DjogiVisage` trait constants (`SCOPE`, `COLUMNS`,
//!   `PROJECTIONS`, `PROJECTION_LIST`) are populated correctly.
//! - The in-memory `From<&Model>` impl produces a visage whose
//!   derived field equals the adopter's Rust expression result.
//! - The `assert_derived_parity` inherent method is emitted and
//!   compiles against a `PartialEq`-bound derived `ty`.
//! - Multiple scopes share a single declaration via
//!   `scopes = [public, admin, export]`.
//!
//! Tier-2 scope (filter / order_by on derived fields) is
//! intentionally NOT exercised — those surfaces land in a follow-up
//! phase per the spec.
//!
//! # Attribute ordering note
//!
//! The `#[model(...)]` attribute is OUTERMOST so it runs first and
//! transforms the struct (injecting framework fields, stripping
//! `#[derived(...)]`) BEFORE `#[derive(Model, Debug, Clone, PartialEq)]`
//! expands. `#[derive(Model)]` is a no-op stub that exists solely to
//! register `derived` as a helper attribute so rustc accepts the
//! `#[derived(...)]` token at parse time; the actual parsing,
//! validation, and stripping all happen inside `#[model(...)]`.

use djogi::DjogiVisage;
use djogi::prelude::*;

// Source struct with three storage columns and one derived projection
// computed from two of them — exactly the spec's motivating scenario.
#[model(table = "phase85_derived_consignments")]
#[derive(Model, Debug, Clone, PartialEq)]
#[derived(
    name   = facility_site,
    ty     = String,
    scopes = [public, admin, export],
    sql    = "CASE WHEN direction = 'inbound' \
                  THEN inbound_site \
                  ELSE outbound_site END",
    rust   = "if model.direction == \"inbound\" { \
                  model.inbound_site.clone() \
              } else { \
                  model.outbound_site.clone() \
              }",
    doc    = " The side of the shipment that is the facility itself.",
)]
pub struct Consignment {
    #[field(expose(public, admin, export))]
    pub inbound_site: String,
    #[field(expose(public, admin, export))]
    pub outbound_site: String,
    #[field(expose(public, admin, export))]
    pub direction: String,
}

fn main() {
    // Scope keys are stable across the four canonical visages.
    assert_eq!(<ConsignmentPublic as DjogiVisage>::SCOPE, "public");
    assert_eq!(<ConsignmentAdmin as DjogiVisage>::SCOPE, "admin");
    assert_eq!(<ConsignmentExport as DjogiVisage>::SCOPE, "export");

    // Framework columns sit at the head of every projection.
    let cols = <ConsignmentPublic as DjogiVisage>::COLUMNS;
    assert!(cols.len() >= 4);
    assert_eq!(cols[0], "id");
    assert_eq!(cols[1], "created_at");
    assert_eq!(cols[2], "updated_at");
    // The derived alias appears at the tail of the projection.
    assert_eq!(cols[cols.len() - 1], "facility_site");

    // PROJECTION_LIST renders column entries verbatim and wraps
    // derived entries with `(<sql>) AS <alias>`.
    let pl = <ConsignmentPublic as DjogiVisage>::PROJECTION_LIST;
    assert!(pl.contains("id"));
    assert!(pl.contains("created_at"));
    assert!(pl.contains("inbound_site"));
    assert!(pl.contains("outbound_site"));
    assert!(pl.contains("direction"));
    assert!(pl.contains("AS facility_site"));
    assert!(pl.contains("CASE WHEN"));

    // Construct a model in-memory via the macro-emitted `Default`
    // impl and round-trip through the visage's infallible
    // `From<&Model>` — exercises the Tier 1 in-memory path without
    // any database I/O.
    let inbound = Consignment {
        inbound_site: "FAC-1".to_string(),
        outbound_site: "WH-2".to_string(),
        direction: "inbound".to_string(),
        ..Default::default()
    };
    let visage: ConsignmentPublic = (&inbound).into();
    assert_eq!(visage.facility_site, "FAC-1");

    let outbound = Consignment {
        inbound_site: "FAC-1".to_string(),
        outbound_site: "WH-2".to_string(),
        direction: "outbound".to_string(),
        ..Default::default()
    };
    let visage_b: ConsignmentPublic = (&outbound).into();
    assert_eq!(visage_b.facility_site, "WH-2");

    // The parity helper short-circuits on derived-field mismatch.
    // Two visages whose `facility_site` values differ must surface
    // `DerivedParityError::Drift`.
    let parity = visage.assert_derived_parity(&visage_b);
    assert!(parity.is_err());

    // Two identically-constructed visages match.
    let visage_c: ConsignmentPublic = (&inbound).into();
    visage.assert_derived_parity(&visage_c).unwrap();
}
