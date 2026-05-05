// Phase 8ε T9.7 — Compile-fail fixture: `set_role` called on a type
// that is NOT `&mut DjogiContext`.
//
// `DjogiContext::set_role` is the ONLY public surface that can issue
// `SET LOCAL ROLE`. The runtime gate inside `set_role` rejects pool-
// backed contexts (returns
// `DjogiError::SetRoleOutsideTransaction`); but the type-system gate
// is what stops adopters from reaching for the underlying
// `tokio_postgres::Client` and bypassing the framework entirely.
//
// This fixture pins the type-system gate. The caller below holds a
// `&tokio_postgres::Client` (the "raw client" most adopters reach
// for when they want to escape the framework) and tries to call
// `.set_role(...)` on it. Rust must reject this with a method-not-
// found error — `set_role` lives on `DjogiContext`, not on the
// underlying client.
//
// Per `feedback_trybuild_fixtures.md`, `fn main() {}` is mandatory so
// the captured `.stderr` does NOT contain `E0601 (main not found)`
// noise alongside the load-bearing receiver-mismatch error.

fn use_set_role(client: &tokio_postgres::Client) {
    // ERROR: `set_role` is not a method on `&tokio_postgres::Client`.
    // The only `set_role` the framework exposes lives on
    // `&mut DjogiContext`. This call site triggers the type-system
    // gate that the runtime gate complements.
    let _ = client.set_role("readonly");
}

fn main() {}
