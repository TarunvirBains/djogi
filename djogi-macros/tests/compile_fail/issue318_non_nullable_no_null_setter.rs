//! Issue #318 — non-nullable fields must NOT gain null-only setters.
//!
//! `name` is a non-`Option` column. The macro emits null-only setters ONLY for
//! outer-`Option<T>` fields. Calling `.name_is_null()` or `.name_is_not_null()`
//! must fail with "method not found". Pins acceptance criterion 2 — a
//! non-nullable column gets no misleading null-only method — and guards against
//! the macro change accidentally widening emission to non-`Option` fields.
//!
//! Two separate functions are used so that both E0599 diagnostics are reported
//! independently, ensuring neither helper is silently emitted.

use djogi::prelude::*;

#[model(table = "issue318_non_nullable")]
#[derive(Debug, Clone)]
pub struct Account {
    // Non-nullable: no null-only setter should exist.
    pub name: String,
}

fn _no_is_null_on_non_nullable() -> AccountFilter {
    // `name` is not `Option<_>` — `name_is_null` must not exist.
    AccountFilter::new().name_is_null()
}

fn _no_is_not_null_on_non_nullable() -> AccountFilter {
    // `name` is not `Option<_>` — `name_is_not_null` must not exist either.
    AccountFilter::new().name_is_not_null()
}

fn main() {}
