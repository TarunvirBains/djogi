//! Emits `impl ::djogi::__private::hooks::HasHooks for #ident {}` and the
//! corresponding sealed-trait witness when `#[model(hooks)]` is set —
//! Phase 8α T1.3.
//!
//! When `model_attrs.hooks == false`, [`expand`] returns an empty
//! [`TokenStream`] without invoking `quote!`. Models that do not opt in
//! emit zero hook-dispatch overhead: the CRUD terminals (T1.4–T1.6) read
//! the `HasHooks` bound at monomorphisation time, so without the impl the
//! dispatch helpers fold to no-ops that LLVM elides regardless of LTO
//! settings (Phase 8 §D2).
//!
//! # Why a separate `Sealed` impl
//!
//! `HasHooks: ModelHooks + ::djogi::__private::hooks::Sealed`. The sealed
//! supertrait lives in a module-private `private` submodule of
//! `djogi::hooks`, re-exported through `::djogi::__private::hooks::Sealed`
//! so macro-emitted code can name it without exposing the seal at the
//! public crate root. Emitting only `impl HasHooks for #ident {}` would
//! fail with E0277 — the seal must be witnessed first.
//!
//! # Why no `impl ModelHooks for #ident {}`
//!
//! The macro cannot prove the adopter wrote `impl ModelHooks for MyModel`
//! at proc-macro expansion time — proc macros run before name resolution,
//! and a sibling `impl` block in the same crate is invisible to the
//! macro. Emitting a blanket `impl ModelHooks for #ident {}` would
//! conflict with the adopter's overriding impl. The opt-in attribute
//! signals intent; the type system enforces the contract: a model with
//! `#[model(hooks)]` but no `impl ModelHooks for M` fails to compile at
//! the use site because the `HasHooks: ModelHooks` supertrait bound goes
//! unsatisfied.

use proc_macro2::TokenStream;
use quote::quote;
use syn::Ident;

/// Emit the `Sealed` + `HasHooks` impl pair when `#[model(hooks)]` is set.
///
/// Returns an empty [`TokenStream`] without invoking `quote!` when the
/// flag is absent so opt-out models pay zero macro-output overhead.
pub fn expand(model_ident: &Ident, model_attrs: &super::attrs::ModelAttrs) -> TokenStream {
    if !model_attrs.hooks {
        return TokenStream::new();
    }
    quote! {
        impl ::djogi::__private::hooks::Sealed for #model_ident {}
        impl ::djogi::__private::hooks::HasHooks for #model_ident {}
    }
}
