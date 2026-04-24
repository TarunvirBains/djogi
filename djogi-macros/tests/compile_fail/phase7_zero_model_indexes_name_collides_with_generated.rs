// Phase 7-Zero v3 T3 — §5 rejection: explicit `name` collides with a
// macro-generated index name (here, the Phase 6 spatial GiST
// `places_location_gix` reserved name).
use djogi::prelude::*;

#[model(table = "places", indexes(
    index(fields = [label], name = "places_location_gix"),
))]
#[derive(Debug, Clone)]
pub struct Place {
    pub label: String,
    pub location: djogi::GeoPoint,
}

fn main() {}
