use djogi::prelude::*;

#[model(table = "visage_derived_null_rows")]
#[derive(Model, Debug, Clone)]
#[derived(
    name = computed_label,
    ty = String,
    scopes = [public],
    sql = "maybe_label",
    rust = "model.maybe_label.clone().unwrap_or_default()",
)]
pub struct DerivedNullRow {
    #[field(expose(public))]
    pub maybe_label: Option<String>,
}

#[model(table = "visage_derived_type_rows")]
#[derive(Model, Debug, Clone)]
#[derived(
    name = computed_label,
    ty = String,
    scopes = [public],
    sql = "count",
    rust = "model.count.to_string()",
)]
pub struct DerivedTypeRow {
    #[field(expose(public))]
    pub count: i32,
}

#[djogi::djogi_test(sync_models = [DerivedNullRow])]
async fn non_optional_derived_null_maps_to_visage_error(mut ctx: DjogiContext) {
    DerivedNullRow::create(
        &mut ctx,
        DerivedNullRow {
            maybe_label: None,
            ..Default::default()
        },
    )
    .await
    .expect("seed nullable source row");

    let err = DerivedNullRowPublic::limit(1)
        .fetch_one(&mut ctx)
        .await
        .expect_err("derived NULL must be rejected for ty = String");

    match err {
        DjogiError::Visage(VisageError::DbComputedNullForNonOptional { visage, field }) => {
            assert_eq!(visage, "DerivedNullRowPublic");
            assert_eq!(field, "computed_label");
        }
        other => panic!("expected derived NULL visage error, got {other:?}"),
    }
}

#[djogi::djogi_test(sync_models = [DerivedTypeRow])]
async fn derived_type_mismatch_maps_to_visage_error(mut ctx: DjogiContext) {
    DerivedTypeRow::create(
        &mut ctx,
        DerivedTypeRow {
            count: 7,
            ..Default::default()
        },
    )
    .await
    .expect("seed typed source row");

    let err = DerivedTypeRowPublic::limit(1)
        .fetch_one(&mut ctx)
        .await
        .expect_err("derived IN must not decode as ty = String");

    match err {
        DjogiError::Visage(VisageError::DbComputedTypeMismatch {
            visage,
            field,
            expected,
            actual,
        }) => {
            assert_eq!(visage, "DerivedTypeRowPublic");
            assert_eq!(field, "computed_label");
            assert!(expected.contains("String"), "expected type was {expected}");
            assert_eq!(actual, "IN");
        }
        other => panic!("expected derived type-mismatch visage error, got {other:?}"),
    }
}
