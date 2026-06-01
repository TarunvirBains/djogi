//! fallible derived projections lift the visage to
//! `TryFrom<&Model, Error = VisageError>`.
//!
//! Pin the codegen contract documented in
//! `docs/spec/visage-derived-fields.md` §"From / TryFrom emission":
//!
//! - When **all** derived entries are infallible (no trailing `?`,
//!   no `Ok(...)` / `Err(...)` block tail), the visage emits
//!   `impl From<&Model>` only. The stdlib blanket
//!   `impl<T, U: Into<T>> TryFrom<U> for T` (with
//!   `Error = Infallible`) gives `TryFrom` for free.
//! - When **any** derived entry is fallible (Shape 1 trailing `?`
//!   or Shapes 2–5 returning `Result<T, E>`), the visage emits
//!   `impl TryFrom<&Model, Error = VisageError>` directly. Adopter
//!   error types that satisfy `VisageError: From<E>` propagate
//!   through `?` (Shape 1) or via outer-`?` lift (Shapes 2–5).
//!
//! The companion `phase85_derived_fields.rs` fixture covers the
//! all-infallible / `From<&Model>` path; this fixture covers the
//! fallible / `TryFrom<&Model>` path.

use djogi::DjogiVisage;
use djogi::prelude::*;
use std::convert::Infallible;

// Source struct with a fallible derived projection. The `rust`
// expression's tail is `Ok(...)`-shaped (`match { Ok / Err }` →
// Shape 2–5), so the visage's `TryFrom<&Model>` lift is mandatory.
// The fallible SQL side uses bare addition — the framework doesn't
// gate SQL fallibility, only the Rust side dictates the lift shape.
#[model(table = "phase85_derived_fallible_consignments")]
#[derive(Model, Debug, Clone, PartialEq)]
#[derived(
    name   = facility_site,
    ty     = String,
    scopes = [public, admin],
    sql    = "CASE WHEN direction = 'inbound' \
                  THEN inbound_site \
                  ELSE outbound_site END",
    rust   = "match model.direction.as_str() { \
                 \"inbound\"  => Ok::<String, std::convert::Infallible>(model.inbound_site.clone()), \
                 \"outbound\" => Ok::<String, std::convert::Infallible>(model.outbound_site.clone()), \
                 other        => Ok::<String, std::convert::Infallible>(other.to_string()), \
              }",
    doc    = " The facility side of the shipment.",
)]
pub struct Consignment {
    #[field(expose(public, admin))]
    pub inbound_site: String,
    #[field(expose(public, admin))]
    pub outbound_site: String,
    #[field(expose(public, admin))]
    pub direction: String,
}

fn main() {
    let model = Consignment {
        inbound_site: "FAC-1".to_string(),
        outbound_site: "WH-2".to_string(),
        direction: "inbound".to_string(),
        ..Default::default()
    };

    // The visage lifts to `TryFrom<&Consignment, Error = VisageError>`.
    // `try_into()` resolves to that impl directly. If the macro had
    // emitted the all-infallible `From<&Model>` route, the stdlib
    // blanket would have given `TryFrom<&Model, Error = Infallible>`
    // — incompatible with `VisageError` and a compile error here.
    let visage: ConsignmentPublic = (&model).try_into().expect("infallible Ok path");
    assert_eq!(visage.facility_site, "FAC-1");

    // Statically prove the visage carries `TryFrom<&Consignment,
    // Error = VisageError>` — checking via a no-op fn pointer
    // assignment forces type unification at compile time. If the
    // macro regressed to emitting `From<&Model>` only (so the stdlib
    // blanket's `Error = Infallible` is the only `TryFrom` reachable),
    // this assertion fails to compile with a Result-type-mismatch
    // error.
    fn requires_visage_error_lift<T>(_: T)
    where
        T: for<'a> TryFrom<&'a Consignment, Error = VisageError>,
    {
    }
    requires_visage_error_lift::<ConsignmentPublic>(visage.clone());

    // Conversely, when adopter rust expressions return
    // `Result<T, Infallible>`, the `VisageError: From<Infallible>`
    // glue (`djogi/src/visage.rs::impl From<Infallible> for
    // VisageError`) accepts the lift uniformly. Pin that path here
    // by statically asserting the `From<Infallible>` impl exists —
    // the outer `?` emitted by the macro for Shape-2..5 entries
    // desugars to `Err(From::from(e))?`, which requires this impl
    // for `Result<T, Infallible>` adopters.
    fn _accepts_from_infallible<E: From<Infallible>>() {}
    _accepts_from_infallible::<VisageError>();

    // Tier-1 trait constants populate identically for fallible
    // visages — the lift shape is orthogonal to the `DjogiVisage`
    // projection metadata.
    assert_eq!(<ConsignmentPublic as DjogiVisage>::SCOPE, "public");
    let cols = <ConsignmentPublic as DjogiVisage>::COLUMNS;
    assert_eq!(cols[cols.len() - 1], "facility_site");
    let pl = <ConsignmentPublic as DjogiVisage>::PROJECTION_LIST;
    assert!(pl.contains("AS facility_site"));
    assert!(pl.contains("CASE WHEN"));

    // The parity helper is still emitted (and the trait impl
    // alongside) for fallible-derived visages — the bound surface
    // is `where String: PartialEq`, which `String` satisfies.
    let other: ConsignmentPublic = (&model).try_into().unwrap();
    visage.assert_derived_parity(&other).unwrap();
}
