//! GH #227 — codec-bearing scalar accessors return the
//! presentation-gated field handle instead of a plain `FieldRef`.
//!
//! A monomorphic local codec keeps the fixture focused on accessor emission
//! instead of the separate generic-codec inventory path.
use djogi::prelude::*;
use djogi::presentation::{
 PresentationCodec, PresentationCodecInfo, Queryability, Reversibility,
 ReversiblePresentationCodec,
};
use djogi::presentation::query::{
 PresentationFieldRef, PresentationOrderCodec, PresentationQueryCodec, PresentationQueryField,
};

pub struct QueryablePlaintextString;

impl PresentationCodecInfo<String> for QueryablePlaintextString {
 type Output = String;
 const REVERSIBILITY: Reversibility = Reversibility::Reversible;
 const QUERYABILITY: Queryability = Queryability::PredicateAndOrder;
}

impl PresentationCodec<String> for QueryablePlaintextString {
 fn present(value: &String) -> String {
  value.clone()
 }
}

impl ReversiblePresentationCodec<String> for QueryablePlaintextString {
 type ReverseError = std::convert::Infallible;

 fn try_reverse(value: &String) -> Result<String, Self::ReverseError> {
  Ok(value.clone())
 }
}

impl PresentationQueryCodec<String> for QueryablePlaintextString {
 type QueryValue = String;

 fn to_query_value_and_build<M: Model>(
  field: PresentationQueryField<M, String>,
  value: String,
 ) -> Q<M> {
  field.eq_storage(value)
 }
}

impl PresentationOrderCodec<String> for QueryablePlaintextString {}

#[model(table = "phase85_227_f3_queryability_identity")]
#[derive(Debug, Clone)]
pub struct User {
 #[field(
  expose(public),
  protected(
   sensitivity = "pii",
   rationale = "public scope intentionally exposes plaintext for this fixture",
   per_scope = {
    public = {
     presentation_codec = QueryablePlaintextString
    }
   }
  )
 )]
 pub ssn: String,
}

fn main() {
 let ssn: PresentationFieldRef<User, QueryablePlaintextString, String> =
  UserPublicFields::default().ssn();
 let _eq: Q<User> = ssn.eq("123-45-6789".to_string());
 let _asc: OrderExpr = ssn.asc();
 let _desc: OrderExpr = ssn.desc();

 let _qs: djogi::query::VisageQuerySet<UserPublic> =
  UserPublic::filter(|f| f.ssn().eq("123-45-6789".to_string()));
}
