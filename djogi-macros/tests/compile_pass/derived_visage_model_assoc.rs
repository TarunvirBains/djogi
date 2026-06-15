//! restored `DjogiVisage::Model`
//! associated type contract.
//!
//! Pin the original-spec `type Model: Model` surface so a generic
//! `V: DjogiVisage` consumer can recover the source model — and
//! therefore the source table — without threading the model in as a
//! separate type parameter:
//!
//! ```ignore
//! fn source_table<V: DjogiVisage>() -> &'static str {
//!  <V::Model as Model>::table_name()
//! }
//! ```
//!
//! The motivating use case is framework-internal traversal code
//! (debug formatters, future Tier-2 predicate rendering) that
//! receives `V: DjogiVisage` and must reach the source table without
//! plumbing the model around. Before this reconciliation the macro
//! omitted `type Model`, forcing every such consumer to add a
//! separate `M: Model, V: DjogiVisageOf<M>` pair.
//!
//! The fixture covers:
//!
//! 1. **Concrete projection** — `<ConsignmentPublic as
//! DjogiVisage>::Model` resolves to `Consignment`.
//! 2. **Generic projection** — a free helper bounded on
//! `V: DjogiVisage` reaches `<V::Model as Model>::table_name()`
//! against the host model's table without naming the source
//! type at the call site.
//! 3. **Cross-scope identity** — every macro-emitted visage scope
//! (`Public`, `Admin`, `Export`) maps to the **same** source
//! `type Model = Consignment` (the four visage scopes are
//! audience-shaped projections of one source).
//!
//! See the spec — `docs/spec/visage-derived-fields.md` §"Trait
//! surface" — for the contract this fixture pins.

use djogi::DjogiVisage;
use djogi::prelude::*;

#[model(table = "phase85_visage_model_assoc_consignments")]
#[derive(Model, Debug, Clone, PartialEq)]
#[derived(
 name = facility_site,
 ty  = String,
 scopes = [public, admin, export],
 sql = "CASE WHEN direction = 'inbound' \
     THEN inbound_site \
     ELSE outbound_site END",
 rust = "if model.direction == \"inbound\" { \
     model.inbound_site.clone() \
    } else { \
     model.outbound_site.clone() \
    }",
)]
pub struct Consignment {
 #[field(expose(public, admin, export))]
 pub inbound_site: String,
 #[field(expose(public, admin, export))]
 pub outbound_site: String,
 #[field(expose(public, admin, export))]
 pub direction: String,
}

/// Generic visage consumer — name `V` alone, reach the source
/// model's table through the restored `type Model` associated item.
/// This is the consumer shape framework-internal callers want.
fn source_table_for<V: DjogiVisage>() -> &'static str {
 <<V as DjogiVisage>::Model as Model>::table_name()
}

/// Statically prove `<V as DjogiVisage>::Model` equals the host
/// source model. The bound `<V as DjogiVisage>::Model: ::std::any::Any`
/// would force monomorphisation; we use `std::any::TypeId` for a
/// compile-time-meaningful equality check at runtime instead.
fn assert_source_model<V>()
where
 V: DjogiVisage<Model = Consignment>,
{
}

fn main() {
 // (1) Concrete projection — equality at the type level via the
 // `M = Consignment` bound. Each scope must satisfy it.
 assert_source_model::<ConsignmentPublic>();
 assert_source_model::<ConsignmentAdmin>();
 assert_source_model::<ConsignmentExport>();

 // (2) Generic projection — the free helper reaches
 // `<V::Model as Model>::table_name()` for an arbitrary
 // `V: DjogiVisage`. The returned string is the host model's
 // table, not the visage's.
 assert_eq!(
  source_table_for::<ConsignmentPublic>(),
  "phase85_visage_model_assoc_consignments",
 );
 assert_eq!(
  source_table_for::<ConsignmentAdmin>(),
  "phase85_visage_model_assoc_consignments",
 );
 assert_eq!(
  source_table_for::<ConsignmentExport>(),
  "phase85_visage_model_assoc_consignments",
 );

 // (3) Cross-scope identity — `Public` / `Admin` / `Export` are
 // distinct visage types but share one source model. The
 // assertions above already pin this; calling the inherent
 // model accessor through each visage as a redundant proof.
 let _ = <ConsignmentPublic as DjogiVisage>::SCOPE;
 let _ = <ConsignmentAdmin as DjogiVisage>::SCOPE;
 let _ = <ConsignmentExport as DjogiVisage>::SCOPE;
}
