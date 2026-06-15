// MirJzSON gate compile-fail fixture.
//
// `#[mirjzson(...)]` is only valid on `MirJzSON` or `Option<MirJzSON>`
// fields. Placing it on any other type — here a `String` — is rejected
// at expand time with a span at the misplaced attribute and a message
// naming the offending field type.

use djogi::prelude::*;

#[model(table = "phase85_mirjzson_attribute_on_wrong_type")]
#[derive(Debug, Clone)]
pub struct WrongType {
 #[mirjzson(justification = "this attribute does not belong on a String")]
 pub title: String,
}

fn main() {}
