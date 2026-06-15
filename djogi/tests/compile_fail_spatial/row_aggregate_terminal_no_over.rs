//! Row-shape aggregate terminals are not column aggregate expressions and
//! must not expose `AggregateExpr::over(...)`.

use djogi::prelude::*;

#[model(table = "phase85_row_aggregate_no_over", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct TileFeature {
 pub name: String,
 pub location: GeoPoint,
}

fn main() {
 let _ = TileFeature::objects()
 .as_mvt_with_options(MvtOptions::new("tiles").with_geom_name("location"))
 .over(|w| w);
}
