//! GH #227 Cluster 3 — mixed-scope presentation-codec conversion behavior.
//!
//! Verifies three things in one mixed-scope fixture:
//! - A `try_presentation_codec` on `public` makes that scope's visage use
//!   `TryFrom<&Model, Error = VisageError>`.
//! - A sibling custom scope without any codec remains infallible and keeps the
//!   storage type.
//! - The public scope output type is non-`String`, while support scope output is
//!   plain `String`, so the test is non-tautological.
use std::convert::Infallible;

use djogi::prelude::*;
use djogi::presentation::{PresentationCodec, PresentationCodecInfo, Queryability, Reversibility, TryPresentationCodec};

pub struct LengthCodec;

#[derive(Debug)]
pub struct LengthCodecError;

impl std::fmt::Display for LengthCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("length codec rejects blank inputs")
    }
}

impl std::error::Error for LengthCodecError {}

impl PresentationCodecInfo<String> for LengthCodec {
    type Output = usize;
    const REVERSIBILITY: Reversibility = Reversibility::OneWay;
    const QUERYABILITY: Queryability = Queryability::Disabled;
}

impl PresentationCodec<String> for LengthCodec {
    fn present(value: &String) -> usize {
        value.len()
    }
}

impl TryPresentationCodec<String> for LengthCodec {
    type Error = LengthCodecError;

    fn try_present(value: &String) -> Result<usize, Self::Error> {
        if value.is_empty() {
            Err(LengthCodecError)
        } else {
            Ok(value.len())
        }
    }
}

#[model(
    table = "phase85_227_per_scope_mixed",
    visage_scopes(support = Support)
)]
#[derive(Debug, Clone)]
pub struct User {
    #[field(
        expose(public, support),
        protected(
            sensitivity = "pii",
            rationale = "public scope is transformed with a fallible codec; support is plaintext",
            per_scope = {
                public = {
                    try_presentation_codec = LengthCodec
                }
            }
        )
    )]
    pub email: String,
}

fn _assert_mixed_scope_conversion(v: &User) {
    // `public` should be a fallible conversion, so the generated API surface
    // includes `TryFrom<&User, Error = VisageError>`.
    let _: Result<UserPublic, djogi::VisageError> = UserPublic::try_from(v);

    // `support` has no codec, so storage stays `String` and conversion stays
    // infallible via `From<&User>` (hence `TryFrom<&User, Error = Infallible>`
    // through the stdlib blanket impl).
    let _ = UserSupport::from(v);
    let _: Result<UserSupport, Infallible> = UserSupport::try_from(v);

    let public: UserPublic = UserPublic::try_from(v).unwrap();
    let _public_len: <LengthCodec as PresentationCodecInfo<String>>::Output = public.email;

    let support: UserSupport = v.into();
    let _support_plain: String = support.email;
}

fn main() {
    let user = User {
        email: "ok".to_string(),
        ..Default::default()
    };
    _assert_mixed_scope_conversion(&user);
}
