// An empty enum must be rejected with a span-precise compile error.
use djogi::DjogiEnum;

#[derive(DjogiEnum, Clone, Copy, PartialEq, Eq, Debug)]
#[djogi_enum(name = "empty_kind")]
pub enum EmptyKind {}

fn main() {}
