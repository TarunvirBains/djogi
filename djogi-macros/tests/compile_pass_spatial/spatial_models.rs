//! Compile-pass fixture: a model with one field per new geometry type.
//! Proves the macro recognizes LineString/Polygon/MultiPoint/MultiPolygon
//! and emits correct FieldSqlType + GiST IndexSpec descriptors.
//!
//! All fields are gated behind `#[cfg(feature = "spatial")]` so the fixture
//! compiles cleanly under default features too (the struct is gated out, the
//! file itself still compiles with just `fn main() {}`).

#[cfg(feature = "spatial")]
use djogi::geo::{LineString, MultiPoint, MultiPolygon, Polygon};
#[cfg(feature = "spatial")]
use djogi::prelude::*;

#[cfg(feature = "spatial")]
#[allow(dead_code)]
#[model(table = "test_all_geometries", no_default)]
#[derive(Debug, Clone)]
pub struct AllGeometries {
 pub path: LineString,
 pub area: Polygon,
 pub stops: MultiPoint,
 pub regions: MultiPolygon,
 pub optional_path: Option<LineString>,
}

fn main() {}
