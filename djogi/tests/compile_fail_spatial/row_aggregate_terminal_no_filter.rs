//! Row-shape aggregate terminals are not column aggregate expressions and
//! must not expose `AggregateExpr::filter(...)`.

use djogi::prelude::*;

#[model(table = "phase85_row_aggregate_no_filter", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct TileFeature {
    pub name: String,
    pub location: GeoPoint,
}

fn main() {
    let _ = TileFeature::objects()
        .as_geobuf("location")
        .filter(Expr::literal(true));
}
