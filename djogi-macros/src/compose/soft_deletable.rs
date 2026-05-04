//! `#[derive(SoftDeletable)]` — emit the `SoftDeletable` trait impl.
//!
//! Phase 8 §T2.3.
//!
//! # What this derive does
//!
//! Given a struct with a declared `deleted_at: Option<DateTime>` field,
//! the derive emits exactly one impl block:
//!
//! ```rust,ignore
//! impl ::djogi::SoftDeletable for #ident {
//!     fn deleted_at(&self) -> ::std::option::Option<::djogi::types::DateTime> {
//!         self.deleted_at
//!     }
//! }
//! ```
//!
//! No fields are added, removed, or renamed. The derive is purely
//! additive at the trait level. Same Path B + same convention-sealed
//! routing as the sibling [`super::auditable`] (Phase 8 §T2.4 —
//! `#[model(auditable)]` opt-in attribute).
//!
//! # Path B — adopter declares the `deleted_at` field
//!
//! Phase 8 v3 line 866 settled the field-injection question on **Path
//! B**: the adopter declares `pub deleted_at: Option<DateTime>` on the
//! struct, and the derive emits only the trait impl. Path A (an
//! attribute macro that mutates the struct AST) was rejected because:
//!
//! 1. Standard Rust `#[derive(...)]` derives **cannot** mutate the
//!    input — see "Risk notes" in the v3 plan and the Rust reference
//!    chapter on derive macros. Forcing field injection would require
//!    a separate `#[soft_deletable]` attribute macro, breaking the
//!    natural "stack the derive next to the existing derives"
//!    composition.
//! 2. Idiomatic Rust derives are non-mutating. Adopters expect
//!    `#[derive(Foo)]` to add an impl, never to silently inject a
//!    field.
//! 3. The adopter cost is one extra line. When the field is missing,
//!    the emitted `self.deleted_at` produces an actionable rustc
//!    diagnostic (`error[E0609]: no field "deleted_at" on type ...`).
//!
//! # Default-filter composition deferred to 8γ T6
//!
//! Phase 8α T2.3 ships **only** the trait impl. The runtime helper
//! `QuerySet::not_deleted()` (added in
//! `djogi/src/query/queryset.rs`) is a manual filter the adopter
//! invokes per `objects()` chain. **Automatic** default-filter
//! composition — making `Model::objects()` exclude soft-deleted rows
//! by default and exposing an `_insecurely()` bypass — is deferred to
//! Phase 8γ T6 once the `Q<T>` substrate lands.
//!
//! Per spec line 971 (RESOLVED 2026-05-03, lens, locked): substrate
//! decisions belong with the substrate refactor; shipping
//! `Model::default_filter()` extension in 8α before `Q<T>` exists in
//! 8γ would create a cross-cluster filter-composition phantom-bug
//! seam — 8α's filter representation would be one shape; 8γ would
//! absorb/rewrite it, and the migration window is exactly when latent
//! bugs surface under long-running adopter use. Simple-to-use is
//! preserved by the manual `.not_deleted()` helper — one extra method
//! call now, automatic by default in 8γ.
//!
//! # Composition with `#[model(...)]`
//!
//! Macro ordering: write `#[derive(SoftDeletable)]` **above**
//! `#[model(...)]`. The derive runs after `#[model]` expansion in the
//! same compilation pass — both produce independent impl blocks that
//! coexist on the same struct without interaction.
//!
//! # Sealing
//!
//! [`djogi::SoftDeletable`] is convention-sealed (doc only — see
//! `djogi/src/compose.rs` module docs for the full rationale). The
//! emitted impl therefore routes through the public re-export
//! `::djogi::SoftDeletable`, not through `::djogi::__private::*`.
//! See `feedback_macro_path_routing.md` for the full routing
//! convention.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{DeriveInput, parse2};

/// Expand `#[derive(SoftDeletable)]` for the given input.
///
/// Returns a compile-error TokenStream on parse failure (covers the
/// rare case where rustc hands the macro a syntactically broken
/// struct after a previous failure recovery).
pub fn expand(input: TokenStream) -> TokenStream {
    let ast: DeriveInput = match parse2(input) {
        Ok(a) => a,
        Err(e) => return e.to_compile_error(),
    };
    let ident = &ast.ident;
    let (impl_generics, ty_generics, where_clause) = ast.generics.split_for_impl();

    quote! {
        impl #impl_generics ::djogi::SoftDeletable for #ident #ty_generics #where_clause {
            fn deleted_at(&self) -> ::std::option::Option<::djogi::types::DateTime> {
                self.deleted_at
            }
        }
    }
}
