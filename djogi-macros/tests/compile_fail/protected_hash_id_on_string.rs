// `redaction = "hash_id"` is only valid on
// fields whose stored type is `HeerId`, `RanjId`, or a custom-PK
// type that the framework can recognise. A plain `String` field is
// the canonical wrong shape: hashing a free-form string with
// HeeRanjId-key material would silently produce hash collisions
// that the schema cannot detect. The macro rejects at parse time so
// the unsafe redaction policy never reaches runtime.
use djogi::prelude::*;

#[model(table = "users")]
#[derive(Debug, Clone)]
pub struct User {
 #[field(protected(
  sensitivity = "pii",
  rationale = "Owner email",
  redaction = "hash_id"
 ))]
 pub email: String,
}

fn main() {}
