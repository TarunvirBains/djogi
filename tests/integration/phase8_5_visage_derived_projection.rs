//! Phase 8.5 issue #231 — visage-derived fields: read-time projection
//! happy-path coverage + parity helper exercise.
//!
//! This is the live integration counterpart to the merged
//! `phase8_5_visage_derived_decode_errors.rs` file. That file
//! exercises ERROR paths (`DbComputedNullForNonOptional`,
//! `DbComputedTypeMismatch`); this file exercises SUCCESS paths plus
//! the parity-helper workflow.
//!
//! Coverage matrix:
//!
//! - **VisageQuerySet round-trip — inbound direction:** create a row
//!   with `direction = "inbound"`, fetch via `ConsignmentPublic::filter`,
//!   assert the derived `facility_site` equals `inbound_site`.
//! - **VisageQuerySet round-trip — outbound direction:** same shape
//!   with `direction = "outbound"`, assert `facility_site` equals
//!   `outbound_site`.
//! - **Sync parity helper (in-memory ↔ fetched):** construct an
//!   in-memory visage via `From<&Model>` and a fetched visage via
//!   `VisageQuerySet`; pass both to the per-visage inherent
//!   `assert_derived_parity` method. Must succeed when SQL and
//!   Rust agree.
//! - **Sync parity helper — deliberate drift detection:** mutate one
//!   visage's derived field and assert the helper surfaces
//!   `DerivedParityError::Drift { field: "facility_site", .. }`.
//! - **Async parity helper convenience:** exercise
//!   `djogi::testing::assert_derived_parity_fetched` to cover the
//!   CTO-required additive async surface (Phase 8.5 #231
//!   reconciliation FIX_BEFORE_BETA-1).
//!
//! Per `feedback_no_raw_execute_in_tests.md`: every test uses
//! `#[djogi_test(sync_models = [...])]` and goes through the typed
//! surface (`Model::create`, `VisageQuerySet`); no `raw_*` escapes.

use djogi::prelude::*;
use djogi::testing::{DerivedParityError, assert_derived_parity_fetched};

/// Consignment model — the canonical motivating scenario from the
/// spec. Three storage columns + one derived projection that picks
/// between `inbound_site` and `outbound_site` based on `direction`.
#[model(table = "phase85_visage_derived_projection_consignments")]
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

#[djogi::djogi_test(sync_models = [Consignment])]
async fn visage_queryset_fetches_inbound_derived_projection(mut ctx: DjogiContext) {
    // Inbound shipment: derived `facility_site` should equal
    // `inbound_site` per the SQL CASE arm.
    let inbound = Consignment::create(
        &mut ctx,
        Consignment {
            inbound_site: "FAC-1".to_string(),
            outbound_site: "WH-2".to_string(),
            direction: "inbound".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("create inbound consignment");

    let fetched = ConsignmentPublic::filter(|f| f.id().eq(inbound.id))
        .fetch_one(&mut ctx)
        .await
        .expect("fetch via VisageQuerySet");

    assert_eq!(
        fetched.facility_site, "FAC-1",
        "derived facility_site must equal inbound_site for direction = inbound"
    );
    // Storage columns round-trip unchanged through the projection.
    assert_eq!(fetched.inbound_site, "FAC-1");
    assert_eq!(fetched.outbound_site, "WH-2");
    assert_eq!(fetched.direction, "inbound");
    assert_eq!(fetched.id, inbound.id);
}

#[djogi::djogi_test(sync_models = [Consignment])]
async fn visage_queryset_fetches_outbound_derived_projection(mut ctx: DjogiContext) {
    // Outbound shipment: derived `facility_site` should equal
    // `outbound_site` per the SQL CASE arm's ELSE branch.
    let outbound = Consignment::create(
        &mut ctx,
        Consignment {
            inbound_site: "FAC-1".to_string(),
            outbound_site: "WH-2".to_string(),
            direction: "outbound".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("create outbound consignment");

    let fetched = ConsignmentPublic::filter(|f| f.id().eq(outbound.id))
        .fetch_one(&mut ctx)
        .await
        .expect("fetch via VisageQuerySet");

    assert_eq!(
        fetched.facility_site, "WH-2",
        "derived facility_site must equal outbound_site for direction = outbound"
    );
}

#[djogi::djogi_test(sync_models = [Consignment])]
async fn assert_derived_parity_succeeds_when_sql_and_rust_agree(mut ctx: DjogiContext) {
    // Standard recommended workflow:
    //   1. Create the model row.
    //   2. Construct in-memory visage via `From<&Model>`.
    //   3. Fetch the visage via `VisageQuerySet`.
    //   4. Compare derived fields with `assert_derived_parity`.
    //
    // The SQL CASE arms and the Rust if/else match each other, so
    // the helper must succeed on every direction.
    let consignment = Consignment::create(
        &mut ctx,
        Consignment {
            inbound_site: "SITE-A".to_string(),
            outbound_site: "SITE-B".to_string(),
            direction: "inbound".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("create consignment");

    let in_memory: ConsignmentPublic = (&consignment).into();
    let from_db: ConsignmentPublic = ConsignmentPublic::filter(|f| f.id().eq(consignment.id))
        .fetch_one(&mut ctx)
        .await
        .expect("fetch via VisageQuerySet");

    in_memory
        .assert_derived_parity(&from_db)
        .expect("derived fields must agree between in-memory and DB-side rendering");

    // Inverse direction also agrees — exercise the ELSE arm too so a
    // future regression in either side surfaces on the next run.
    let outbound = Consignment::create(
        &mut ctx,
        Consignment {
            inbound_site: "SITE-A".to_string(),
            outbound_site: "SITE-B".to_string(),
            direction: "outbound".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("create outbound consignment");

    let outbound_in_memory: ConsignmentPublic = (&outbound).into();
    let outbound_from_db: ConsignmentPublic = ConsignmentPublic::filter(|f| f.id().eq(outbound.id))
        .fetch_one(&mut ctx)
        .await
        .expect("fetch outbound visage");

    outbound_in_memory
        .assert_derived_parity(&outbound_from_db)
        .expect("outbound direction parity must hold");
}

#[djogi::djogi_test(sync_models = [Consignment])]
async fn assert_derived_parity_detects_drift_on_changed_derived_value(mut ctx: DjogiContext) {
    // Hand-construct two visages whose derived `facility_site`
    // values disagree, then assert the helper short-circuits at the
    // first mismatched field. We do NOT need a DB write to exercise
    // the drift path — the helper is comparing in-memory state.
    //
    // The framework guarantee is: any difference on a derived field
    // surfaces as `DerivedParityError::Drift { field: <name>, .. }`.
    // The helper does NOT compare framework columns (`id`,
    // `created_at`, `updated_at`) or storage columns — they are
    // populated identically from the same `&Model`.
    let consignment = Consignment::create(
        &mut ctx,
        Consignment {
            inbound_site: "SITE-A".to_string(),
            outbound_site: "SITE-B".to_string(),
            direction: "inbound".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("create consignment");

    let truthful: ConsignmentPublic = (&consignment).into();
    let mut drifted = truthful.clone();
    drifted.facility_site = "DRIFTED-SITE".to_string();

    let err = truthful
        .assert_derived_parity(&drifted)
        .expect_err("derived drift must surface as Err");

    match err {
        DerivedParityError::Drift { visage, field } => {
            assert_eq!(visage, "ConsignmentPublic");
            assert_eq!(field, "facility_site");
        }
        other => panic!("expected Drift variant, got {other:?}"),
    }

    // Storage-column drift must NOT surface — the helper compares
    // derived fields only. Mutate a storage column on the drifted
    // visage and re-run the helper: it must still pass.
    let mut storage_drifted = truthful.clone();
    storage_drifted.inbound_site = "DIFFERENT".to_string();
    truthful
        .assert_derived_parity(&storage_drifted)
        .expect("storage-column differences must NOT trip the parity helper");
}

#[djogi::djogi_test(sync_models = [Consignment])]
async fn assert_derived_parity_fetched_async_helper(mut ctx: DjogiContext) {
    // The CTO-required additive async convenience surface
    // (Phase 8.5 #231 reconciliation FIX_BEFORE_BETA-1).
    //
    // Workflow:
    //   1. Create the model row.
    //   2. Construct in-memory visage via `From<&Model>`.
    //   3. Call `assert_derived_parity_fetched(in_memory, || fetch_future)`
    //      and let the helper drive the fetch + delegate to the sync
    //      per-visage method.
    let consignment = Consignment::create(
        &mut ctx,
        Consignment {
            inbound_site: "SITE-X".to_string(),
            outbound_site: "SITE-Y".to_string(),
            direction: "outbound".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("create consignment");

    let in_memory: ConsignmentPublic = (&consignment).into();
    let target_id = consignment.id;

    // Happy path — derived field round-trips, helper resolves Ok.
    assert_derived_parity_fetched(&in_memory, || async {
        ConsignmentPublic::filter(|f| f.id().eq(target_id))
            .fetch_one(&mut ctx)
            .await
    })
    .await
    .expect("async fetched parity must succeed when SQL and Rust agree");

    // Drift path — change the in-memory derived field and re-run
    // through the async helper. Helper must surface the same
    // Drift variant as the sync per-visage method.
    let mut drifted = in_memory.clone();
    drifted.facility_site = "ASYNC-DRIFT".to_string();
    let err = assert_derived_parity_fetched(&drifted, || async {
        ConsignmentPublic::filter(|f| f.id().eq(target_id))
            .fetch_one(&mut ctx)
            .await
    })
    .await
    .expect_err("async drift must surface as Err");

    match err {
        DerivedParityError::Drift { visage, field } => {
            assert_eq!(visage, "ConsignmentPublic");
            assert_eq!(field, "facility_site");
        }
        other => panic!("expected Drift variant from async helper, got {other:?}"),
    }
}

#[test]
fn visage_descriptor_inventory_registers_per_scope() {
    // Phase 8.5 #231 BLOCK-1 — the descriptor inventory.
    //
    // `#[model] + #[derived]` emits one `inventory::submit!(VisageDescriptor)`
    // per `(Model, scope)` pair that has at least one derived entry in
    // scope. The `Consignment` declared above lists
    // `scopes = [public, admin, export]` for its single derived
    // `facility_site` entry, so exactly THREE descriptors land in the
    // global `inventory::iter::<VisageDescriptor>()` collection for
    // this model — one per scope.
    //
    // The collection is **structurally separate** from
    // `ModelDescriptor` / `EnumDescriptor` — the migration differ
    // never observes derived projections (the storage-vs-projection
    // split is mechanical, not conventional). This test pins the
    // emission contract.
    let descriptors: Vec<&::djogi::descriptor::VisageDescriptor> =
        ::inventory::iter::<::djogi::descriptor::VisageDescriptor>()
            .filter(|d| d.model_name == "Consignment")
            .collect();

    // Three scopes contain the derived entry; SelfView does not, so
    // no descriptor is emitted for it.
    let mut scopes: Vec<&str> = descriptors.iter().map(|d| d.scope).collect();
    scopes.sort();
    assert_eq!(scopes, vec!["admin", "export", "public"]);

    // Each descriptor carries exactly one DerivedProjection entry
    // (`facility_site`).
    for d in &descriptors {
        assert_eq!(d.derived.len(), 1);
        let entry = &d.derived[0];
        assert_eq!(entry.name, "facility_site");
        assert_eq!(entry.ty_path.trim(), "String");
        assert!(entry.sql.contains("CASE WHEN direction"));
        assert!(entry.rust.contains("model.inbound_site"));
        // Originating scopes carried verbatim in source order.
        assert_eq!(entry.scopes, &["public", "admin", "export"]);
        // The `doc = "..."` literal was captured.
        assert_eq!(
            entry.doc,
            Some(" The side of the shipment that is the facility itself.")
        );
    }

    // Visage names round-trip through the descriptor.
    let mut visage_names: Vec<&str> = descriptors.iter().map(|d| d.visage_name).collect();
    visage_names.sort();
    assert_eq!(
        visage_names,
        vec!["ConsignmentAdmin", "ConsignmentExport", "ConsignmentPublic"]
    );
}

/// Phase 8.5 #231 reconciliation — pin the restored `DjogiVisage::Model`
/// associated-type contract from the live integration side.
///
/// The compile_pass fixture
/// `djogi-macros/tests/compile_pass/derived_visage_model_assoc.rs`
/// pins the trait surface against the proc-macro emission; this test
/// pins the same contract against the framework's runtime side by
/// exercising `<V::Model as Model>::table_name()` against a live
/// `VisageQuerySet` round-trip. A generic visage consumer can recover
/// the source model — and therefore the source table — without
/// threading the model in as a separate type parameter.
///
/// Coverage:
///
/// 1. **Per-scope consistency** — every emitted visage scope
///    (`Public`, `Admin`, `Export`) maps to the same source
///    `type Model = Consignment` and therefore the same source
///    table name.
/// 2. **Generic free helper** — a `fn source_table<V: DjogiVisage>()`
///    free helper resolves `<V::Model as Model>::table_name()` at the
///    type level, no `M:` parameter, no inference burden at the call
///    site. This is the framework-internal consumer shape the
///    original #231 acceptance criteria targeted.
#[test]
fn djogi_visage_model_assoc_recovers_source_table() {
    use djogi::DjogiVisage;

    fn source_table_for<V: DjogiVisage>() -> &'static str {
        <<V as DjogiVisage>::Model as Model>::table_name()
    }

    let expected = "phase85_visage_derived_projection_consignments";
    assert_eq!(source_table_for::<ConsignmentPublic>(), expected);
    assert_eq!(source_table_for::<ConsignmentAdmin>(), expected);
    assert_eq!(source_table_for::<ConsignmentExport>(), expected);

    // Pin the type-level identity directly — a bound on `V: DjogiVisage<Model = Consignment>`
    // forces compile-time equality with the source model. The body is
    // empty; the bound IS the test. The accept-block can never
    // monomorphise against any other model.
    fn accepts<V>()
    where
        V: DjogiVisage<Model = Consignment>,
    {
    }
    accepts::<ConsignmentPublic>();
    accepts::<ConsignmentAdmin>();
    accepts::<ConsignmentExport>();
}
