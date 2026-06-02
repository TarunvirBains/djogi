// #92 — live PostGIS coverage for row-shape
// aggregate terminals (`ST_AsMVT` / `ST_AsGeobuf`).

use djogi::geo::GeoPoint;
use djogi::prelude::*;

#[model(table = "phase85_c4f_tile_features", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct TileFeature {
    pub name: String,
    pub location: GeoPoint,
}

fn tile_feature(name: &str, lat: f64, lon: f64) -> TileFeature {
    TileFeature {
        id: djogi::HeerId::from_i64(0).expect("HeerId sentinel"),
        created_at: djogi::DateTime::UNIX_EPOCH,
        updated_at: djogi::DateTime::UNIX_EPOCH,
        name: name.to_string(),
        location: GeoPoint::new(lat, lon).expect("valid coordinate"),
    }
}

#[djogi::djogi_test(extensions = ["postgis"], sync_models = [TileFeature])]
async fn queryset_as_mvt_returns_bytea_vec(mut ctx: djogi::DjogiContext) {
    TileFeature::create(&mut ctx, tile_feature("sfo", 37.62131, -122.37896))
        .await
        .expect("create SFO feature");
    TileFeature::create(&mut ctx, tile_feature("jfk", 40.64131, -73.77814))
        .await
        .expect("create JFK feature");

    let bytes: Vec<u8> = TileFeature::objects()
        .as_mvt_with_options(
            MvtOptions::new("airports")
                .with_geom_name("location")
                .with_feature_id_name("id"),
        )
        .fetch_one(&mut ctx)
        .await
        .expect("ST_AsMVT terminal must execute");

    assert!(
        !bytes.is_empty(),
        "MVT bytea should decode as non-empty Vec<u8>"
    );
}

#[djogi::djogi_test(extensions = ["postgis"], sync_models = [TileFeature])]
async fn queryset_as_mvt_none_short_circuit(mut ctx: djogi::DjogiContext) {
    let bytes: Vec<u8> = TileFeature::objects()
        .none()
        .as_mvt("airports")
        .fetch_one(&mut ctx)
        .await
        .expect("none queryset should short-circuit to an empty result");

    assert!(
        bytes.is_empty(),
        "None queryset must return Ok(Vec::new()) without issuing SQL"
    );
}

#[djogi::djogi_test(extensions = ["postgis"], sync_models = [TileFeature])]
async fn annotated_as_mvt_returns_bytea_vec(mut ctx: djogi::DjogiContext) {
    TileFeature::create(&mut ctx, tile_feature("sfo", 37.62131, -122.37896))
        .await
        .expect("create SFO feature");

    let bytes: Vec<u8> = TileFeature::objects()
        .annotate(|f| f.id().count_star())
        .as_mvt_with_options(MvtOptions::new("airports").with_geom_name("location"))
        .fetch_one(&mut ctx)
        .await
        .expect("annotated ST_AsMVT terminal must execute");

    assert!(!bytes.is_empty(), "annotated MVT should be non-empty");
}

#[djogi::djogi_test(extensions = ["postgis"], sync_models = [TileFeature])]
async fn queryset_as_mvt_zero_rows_is_ok(mut ctx: djogi::DjogiContext) {
    let bytes: Vec<u8> = TileFeature::objects()
        .filter(|f| f.name().eq("nonexistent".to_string()))
        .as_mvt_with_options(MvtOptions::new("airports").with_geom_name("location"))
        .fetch_one(&mut ctx)
        .await
        .expect("zero-row MVT filter must not panic and must return bytea");

    assert!(
        bytes.is_empty(),
        "zero-row MVT filter should normalize SQL NULL to an empty Vec"
    );
}

#[djogi::djogi_test(extensions = ["postgis"], sync_models = [TileFeature])]
async fn queryset_as_geobuf_returns_bytea_vec(mut ctx: djogi::DjogiContext) {
    TileFeature::create(&mut ctx, tile_feature("sfo", 37.62131, -122.37896))
        .await
        .expect("create SFO feature");

    let bytes: Vec<u8> = TileFeature::objects()
        .as_geobuf("location")
        .fetch_one(&mut ctx)
        .await
        .expect("ST_AsGeobuf terminal must execute");

    assert!(
        !bytes.is_empty(),
        "Geobuf bytea should decode as non-empty Vec<u8>"
    );
}

#[djogi::djogi_test(extensions = ["postgis"], sync_models = [TileFeature])]
async fn queryset_as_geobuf_none_short_circuit(mut ctx: djogi::DjogiContext) {
    let bytes: Vec<u8> = TileFeature::objects()
        .none()
        .as_geobuf("location")
        .fetch_one(&mut ctx)
        .await
        .expect("none queryset should short-circuit to an empty result");

    assert!(
        bytes.is_empty(),
        "None queryset must return Ok(Vec::new()) without issuing SQL"
    );
}

#[djogi::djogi_test(extensions = ["postgis"], sync_models = [TileFeature])]
async fn queryset_as_geobuf_zero_rows_is_ok(mut ctx: djogi::DjogiContext) {
    let bytes: Vec<u8> = TileFeature::objects()
        .filter(|f| f.name().eq("nonexistent".to_string()))
        .as_geobuf("location")
        .fetch_one(&mut ctx)
        .await
        .expect("zero-row Geobuf filter must not panic or decode NULL");

    assert!(
        bytes.is_empty(),
        "zero-row Geobuf filter should normalize SQL NULL to an empty Vec"
    );
}
