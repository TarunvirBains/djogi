//! Compile-pass: a #[model(soft_deletable)] model's objects_including_deleted()
//! compiles from adopter-crate context. This pins that
//! QuerySet::__new_with_explicit_condition is #[doc(hidden)] pub (not
//! pub(crate)). If the ctor were pub(crate), the macro emission would fail
//! E0624 for every soft-deletable model downstream, but the error would be
//! invisible to in-crate integration tests (which see pub(crate) normally).
//!
//! The full cross-crate emission chain exercised here:
//!   macro expands in fixture crate
//!     -> objects_including_deleted() body calls
//!        QuerySet::<SdBypassFixture>::__new_with_explicit_condition(...)
//!     -> that ctor must be `pub` for the fixture to compile.
//!
//! Every lihaaf compile-fixture must have `fn main()` so the binary links.

use djogi::prelude::*;

#[model(table = "sd_bypass_fixture", soft_deletable)]
#[derive(Debug, Clone)]
pub struct SdBypassFixture {
    pub note: String,
    pub deleted_at: Option<djogi::DateTime>,
}

fn _calls_emitted_bypass() -> djogi::query::QuerySet<SdBypassFixture> {
    SdBypassFixture::objects_including_deleted()
}

fn main() {
    let _qs = _calls_emitted_bypass();
}
