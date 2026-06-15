// A tuple variant must be rejected with a span-precise compile error.
use djogi::DjogiEnum;

#[derive(DjogiEnum, Clone, PartialEq, Eq, Debug)]
#[djogi_enum(name = "shape_kind")]
pub enum ShapeKind {
 Circle(u32),
 Square,
}

fn main() {}
