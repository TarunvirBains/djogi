//! GH #227 — non-queryable presentation codecs must not leak the
//! raw `FieldRef` predicate surface through `{Visage}Fields` accessors.
//!
//! `MaskString` is queryability-disabled, so `.eq(...)` must be absent on the
//! generated accessor handle.
use djogi::prelude::*;
use djogi::presentation::builtins::MaskString;

#[model(table = "phase85_227_f3_queryability_disabled_eq")]
#[derive(Debug, Clone)]
pub struct User {
 #[field(
  expose(public),
  protected(
   sensitivity = "pii",
   rationale = "public scope masks the value in this fixture",
   per_scope = {
    public = {
     presentation_codec = MaskString
    }
   }
  )
 )]
 pub ssn: String,
}

fn main() {
 let _bad = UserPublicFields::default().ssn().eq("123-45-6789".to_string());
}
