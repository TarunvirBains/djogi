// Internal pg_catalog coverage for `#[djogi_test(sync_models = [...])]`.
//
// The ordinary live test now proves adopter-visible behavior through typed
// CRUD/query APIs. These tests preserve the original direct catalog assertions
// for table, index, FK, JSONB, and PostGIS type metadata.

use djogi::prelude::*;
use djogi::relation::ForeignKey;

#[derive(djogi::JsonbSchema, serde::Serialize, serde::Deserialize, Default, Debug, Clone)]
pub struct CatalogPrefs {
    pub theme: String,
}

#[model(table = "t10_catalog_widgets_solo", pk = HeerId, indexes(index(fields = [name])))]
#[derive(Debug, Clone)]
pub struct CatalogWidgetSolo {
    pub name: String,
    pub price_cents: i32,
}

#[model(table = "t10_catalog_categories", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct CatalogCategory {
    pub name: String,
}

#[model(table = "t10_catalog_widgets", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct CatalogWidget {
    pub category_id: ForeignKey<CatalogCategory>,
    pub name: String,
}

#[model(table = "t10_catalog_users_prefs", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct CatalogUserWithPrefs {
    pub email: String,
    pub prefs: Jsonb<CatalogPrefs>,
}

#[cfg(feature = "spatial")]
#[model(table = "t10_catalog_places", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct CatalogPlace {
    pub name: String,
    pub location: djogi::GeoPoint,
}

#[model(table = "t10_catalog_tags", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct CatalogTag {
    pub label: String,
}

#[model(table = "t10_catalog_posts", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct CatalogPost {
    pub title: String,
}

#[model(table = "t10_catalog_post_tags", pk = HeerId, through, no_default)]
#[derive(Debug, Clone)]
pub struct CatalogPostTag {
    pub post_id: ForeignKey<CatalogPost>,
    pub tag_id: ForeignKey<CatalogTag>,
}

djogi::many_to_many!(
    CatalogPost,
    CatalogTag,
    through = CatalogPostTag,
    this_fk = post_id,
    that_fk = tag_id,
    relation = "tags"
);

#[djogi::djogi_test(sync_models = [CatalogWidgetSolo])]
async fn single_model_sync_catalog_has_table_and_declared_index(mut ctx: DjogiContext) {
    let table_count: i64 = ctx
        .raw_scalar(
            "SELECT count(*)::bigint FROM pg_class \
             WHERE relname = 't10_catalog_widgets_solo' AND relkind = 'r'",
            &[],
        )
        .await
        .expect("pg_class lookup");
    assert_eq!(table_count, 1, "sync_models must create the table");

    let idx_count: i64 = ctx
        .raw_scalar(
            "SELECT count(*)::bigint FROM pg_indexes \
             WHERE tablename = 't10_catalog_widgets_solo'",
            &[],
        )
        .await
        .expect("pg_indexes lookup");
    assert!(
        idx_count >= 2,
        "expected at least 2 pg_indexes entries: the PRIMARY KEY constraint's \
         implicit unique BTree index (Postgres-managed, NOT a djogi-emitted \
         CREATE INDEX) plus the user-declared name index; got {idx_count}",
    );
}

#[djogi::djogi_test(sync_models = [CatalogWidget, CatalogCategory])]
async fn fk_dependency_sync_catalog_has_referential_constraint(mut ctx: DjogiContext) {
    let fk_count: i64 = ctx
        .raw_scalar(
            "SELECT count(*)::bigint FROM pg_constraint c \
             JOIN pg_class src ON src.oid = c.conrelid \
             JOIN pg_class tgt ON tgt.oid = c.confrelid \
             WHERE c.contype = 'f' \
               AND src.relname = 't10_catalog_widgets' \
               AND tgt.relname = 't10_catalog_categories'",
            &[],
        )
        .await
        .expect("pg_constraint lookup");
    assert_eq!(
        fk_count, 1,
        "expected one FK from t10_catalog_widgets to t10_catalog_categories",
    );
}

#[djogi::djogi_test(sync_models = [CatalogUserWithPrefs])]
async fn jsonb_field_sync_catalog_has_jsonb_column(mut ctx: DjogiContext) {
    let column_type: String = ctx
        .raw_scalar(
            "SELECT data_type FROM information_schema.columns \
             WHERE table_name = 't10_catalog_users_prefs' AND column_name = 'prefs'",
            &[],
        )
        .await
        .expect("prefs column data_type lookup");
    assert_eq!(column_type, "jsonb", "Jsonb<T> must lower to jsonb");
}

#[cfg(feature = "spatial")]
#[djogi::djogi_test(extensions = ["postgis"], sync_models = [CatalogPlace])]
async fn spatial_field_sync_catalog_has_geography_column(mut ctx: DjogiContext) {
    let udt: String = ctx
        .raw_scalar(
            "SELECT udt_name FROM information_schema.columns \
             WHERE table_name = 't10_catalog_places' AND column_name = 'location'",
            &[],
        )
        .await
        .expect("information_schema lookup");
    assert_eq!(udt, "geography");
}

#[djogi::djogi_test(sync_models = [CatalogTag, CatalogPost, CatalogPostTag])]
async fn m2m_through_sync_catalog_has_two_endpoint_fks(mut ctx: DjogiContext) {
    for table in [
        "t10_catalog_tags",
        "t10_catalog_posts",
        "t10_catalog_post_tags",
    ] {
        let exists: i64 = ctx
            .raw_scalar(
                "SELECT count(*)::bigint FROM pg_class \
                 WHERE relname = $1 AND relkind = 'r'",
                &[&table],
            )
            .await
            .expect("pg_class lookup");
        assert_eq!(exists, 1, "table {table} must be created by sync_models");
    }

    let fk_count: i64 = ctx
        .raw_scalar(
            "SELECT count(*)::bigint FROM pg_constraint c \
             JOIN pg_class src ON src.oid = c.conrelid \
             WHERE c.contype = 'f' AND src.relname = 't10_catalog_post_tags'",
            &[],
        )
        .await
        .expect("pg_constraint lookup");
    assert_eq!(
        fk_count, 2,
        "junction table must have one FK per endpoint",
    );
}
