//! Phase 4.5 — projection annotation forms must all parse cleanly.
//!
//! Compile-pass baseline for every legal form of `#[field(expose(...))]`.
//! If any form regresses (parser rejects it, codegen fails on a valid
//! combination), this file stops compiling and trybuild surfaces it.
//!
//! Task 2 delivers the parser only — the projection structs themselves
//! land in Task 3. This fixture therefore only exercises the parse path;
//! it does NOT yet reference `UserPublic`, `UserAdmin`, etc.
use djogi::prelude::*;

#[model(table = "users_pass_phase4_5")]
#[derive(Debug, Clone)]
pub struct User {
    // Scalar, single scope.
    #[field(expose(public))]
    pub display_name: String,

    // Scalar, multiple scopes.
    #[field(expose(self_view, admin, export))]
    pub email: String,

    // Absent — default internal.
    pub password_hash: String,

    // Explicit no-op sentinels (both accepted).
    #[field(expose(none))]
    pub internal_notes: String,

    #[field(expose(internal))]
    pub salt: String,

    // Scalar, all four scopes.
    #[field(expose(public, self_view, admin, export))]
    pub handle: String,
}

fn main() {}
