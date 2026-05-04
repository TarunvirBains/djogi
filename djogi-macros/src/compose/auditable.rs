//! `#[derive(Auditable)]` — emit the `Auditable` trait impl.
//!
//! Phase 8 §T2.2.
//!
//! # What this derive does
//!
//! Given a struct with a declared `created_by: Option<String>` field, the
//! derive emits exactly one impl block:
//!
//! ```rust,ignore
//! impl ::djogi::Auditable for #ident {
//!     fn created_by(&self) -> ::std::option::Option<&str> {
//!         self.created_by.as_deref()
//!     }
//! }
//! ```
//!
//! No fields are added, removed, or renamed. The derive is purely
//! additive at the trait level.
//!
//! # Path B — adopter declares the `created_by` field
//!
//! Phase 8 v3 line 866 settled the field-injection question on **Path
//! B**: the adopter declares `pub created_by: Option<String>` on the
//! struct, and the derive emits only the trait impl. Path A (an
//! attribute macro that mutates the struct AST) was rejected because:
//!
//! 1. Standard Rust `#[derive(...)]` derives **cannot** mutate the
//!    input — see "Risk notes" in the v3 plan and Rust reference
//!    chapter on derive macros. Forcing field injection would require
//!    a separate `#[auditable]` attribute macro, breaking the natural
//!    "stack the derive next to the existing derives" composition.
//! 2. Idiomatic Rust derives are non-mutating. Adopters expect
//!    `#[derive(Foo)]` to add an impl, never to silently inject a
//!    field.
//! 3. The adopter cost is one extra line. When the field is missing,
//!    the emitted `self.created_by.as_deref()` produces an
//!    actionable rustc diagnostic (`error[E0609]: no field
//!    "created_by" on type ...`).
//!
//! If a future phase finds the cost-of-friction high enough to
//! warrant auto-injection, a sibling `#[auditable(inject)]` attribute
//! macro can be added without breaking the existing derive — the two
//! surfaces compose orthogonally.
//!
//! # Composition with `#[model(hooks)]`
//!
//! `#[derive(Auditable)]` is purely a getter — population of
//! `created_by` from [`djogi::DjogiContext::auth`] requires the
//! adopter to also write `#[model(hooks)]` and an `impl ModelHooks`
//! with a `before_create` body that captures `auth.user_id()` into
//! `self.created_by`. T2.4 will land a helper that synthesises that
//! body; T2.2 (this commit) is getter-only.
//!
//! Macro ordering: write `#[derive(Auditable)]` **above**
//! `#[model(...)]`. The derive runs after `#[model]` expansion in the
//! same compilation pass — both produce independent impl blocks that
//! coexist on the same struct without interaction.
//!
//! # Sealing
//!
//! [`djogi::Auditable`] is convention-sealed (doc only — see
//! `djogi/src/compose.rs` module docs for the full rationale). The
//! emitted impl therefore routes through the public re-export
//! `::djogi::Auditable`, not through `::djogi::__private::*`. T1.3's
//! `HasHooks` impl uses the `__private` route because `HasHooks` is
//! supertrait-sealed via `__private::hooks::Sealed`; `Auditable` is
//! not, so the canonical public path is correct. See
//! `feedback_macro_path_routing.md` for the full routing convention.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{DeriveInput, parse2};

/// Expand `#[derive(Auditable)]` for the given input.
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
        impl #impl_generics ::djogi::Auditable for #ident #ty_generics #where_clause {
            fn created_by(&self) -> ::std::option::Option<&str> {
                self.created_by.as_deref()
            }
        }
    }
}
