//! `#[djogi::trait_impl]` attribute macro — Phase 8β T5.2 + T5.3.
//!
//! Wraps a trait `impl` block with a sibling `inventory::submit!`
//! registration so cross-cutting consumers (`Sassi::all_impl::<dyn T>()`,
//! T5.4 + 8δ T7) can iterate every model that implements a given
//! trait without naming each model in the consumer's path.
//!
//! T5.2 ships the parser + the impl-block round-trip; T5.3 fills in
//! the type-erased caster body. This file currently implements T5.2
//! only — the emitted `caster` field is a placeholder that returns
//! `None` for every input (the registration is reachable via
//! `iter_for_trait::<dyn T>` but downcasting to a concrete `Arc<dyn T>`
//! is an explicit T5.3 task).
//!
//! # Why a separate attribute macro
//!
//! Adopters writing `impl Searchable for Vehicle { ... }` register
//! with one attribute prefix:
//!
//! ```ignore
//! #[djogi::trait_impl]
//! impl Searchable for Vehicle {
//!     fn searchable_columns(&self) -> &[&'static str] { &["title"] }
//! }
//! ```
//!
//! No additional code at the impl block; the macro emits a
//! `inventory::submit!(TraitRegistration { ... })` alongside the
//! unchanged impl block. The impl block reaches rustc verbatim so
//! adopter-side compile errors (e.g. wrong method signature) point
//! at the adopter's own code, not at the macro expansion.
//!
//! # What we accept
//!
//! - Trait impls only — `impl Trait for Type { ... }`. Inherent
//!   `impl Type { ... }` rejected.
//! - Concrete (non-generic) impls only — `impl<T> Trait for Vec<T>`
//!   rejected. Generic impls would require parameter substitution
//!   for the `TypeId::of` lookup at registration time, which is
//!   deferred to a future phase per `feedback_anchored_deferrals`.
//! - Single-segment `Self` types — `impl Trait for Vehicle` works,
//!   `impl Trait for crate::module::Vehicle` works (path resolved
//!   verbatim), `impl Trait for some_fn()::Vehicle` rejected (not a
//!   nameable type at parse time).

use proc_macro2::TokenStream;
use quote::{ToTokens, format_ident, quote};
use syn::ItemImpl;

/// Entry point — parses the attribute macro input + the impl block,
/// validates the shape, and emits the impl unchanged plus the
/// `inventory::submit!` registration block.
pub fn expand(_attr: TokenStream, item: TokenStream) -> TokenStream {
    match try_expand(item) {
        Ok(ts) => ts,
        Err(err) => err.to_compile_error(),
    }
}

fn try_expand(item: TokenStream) -> syn::Result<TokenStream> {
    // Parse the impl block. If the user typed something that does
    // not parse as an impl block at all, surface the syn error
    // verbatim so rustc points at the offending token.
    let item_impl: ItemImpl = syn::parse2(item.clone())?;

    // Reject inherent impls (no trait).
    let trait_path_pair = item_impl.trait_.as_ref().ok_or_else(|| {
        syn::Error::new_spanned(
            &item_impl,
            "`#[djogi::trait_impl]` requires a trait impl — \
             inherent impl blocks (`impl Type { ... }`) cannot be \
             registered for cross-type queries; rewrite as \
             `impl SomeTrait for Type { ... }`",
        )
    })?;
    // syn::ItemImpl::trait_ is `Option<(Option<!>, Path, Token![for])>`.
    let trait_path = &trait_path_pair.1;

    // Reject generic impls (`impl<T> Trait for Vec<T>`). T5.2 / T5.3
    // only handle non-generic concrete impls in v0.1.0; generic impls
    // would require runtime parameter substitution for the
    // `TypeId::of` lookup which is deferred to a future phase per
    // `feedback_anchored_deferrals`.
    if !item_impl.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &item_impl.generics,
            "`#[djogi::trait_impl]` does not yet support generic impls \
             — concrete `impl Trait for ConcreteType { ... }` only in \
             v0.1.0; generic impls deferred to a future phase",
        ));
    }

    // Self-type (the model). `*item_impl.self_ty` is a `syn::Type`;
    // we accept any path-shaped type (single or multi-segment) and
    // emit `<#self_ty as ::core::any::Any>::type_id` lookups at the
    // registration site. Non-path shapes (tuples, references,
    // function pointers) are rejected here because they cannot
    // satisfy `'static` for `TypeId::of`.
    let self_ty = &*item_impl.self_ty;
    if !matches!(self_ty, syn::Type::Path(_)) {
        return Err(syn::Error::new_spanned(
            self_ty,
            "`#[djogi::trait_impl]`'s self type must be a named type \
             — tuple, reference, or function-pointer types are not \
             supported",
        ));
    }

    // Render the model + trait names for the descriptor's debug
    // fields. Strip surrounding whitespace so the rendered names
    // match adopter expectations (e.g. `Vehicle`, not ` Vehicle `).
    let model_type_name = self_ty.to_token_stream().to_string().replace(' ', "");
    let trait_type_name = trait_path.to_token_stream().to_string().replace(' ', "");

    // Generate a unique identifier for the static-string consts so
    // multiple `#[djogi::trait_impl]` blocks in the same module do
    // not collide on a shared name. Format: `__djogi_trait_impl_caster_<Model>_<TraitLast>`.
    // The last segment of the trait path is enough to disambiguate
    // since the same model cannot register the same trait twice.
    let trait_last_seg = trait_path
        .segments
        .last()
        .map(|s| s.ident.to_string())
        .unwrap_or_else(|| "Trait".to_string());
    let caster_fn_ident = format_ident!(
        "__djogi_trait_impl_caster_{}_{}",
        model_type_name.replace("::", "_"),
        trait_last_seg,
    );

    // T5.2 emission — the impl block unchanged + the registration
    // block. The `caster` field is currently a placeholder that
    // returns `None` for every input; T5.3 fills in the real body.
    Ok(quote! {
        #item_impl

        // Type-erased caster placeholder — T5.2 ships an always-
        // `None` body; T5.3 fills in the safe carrier pattern.
        // The function is `pub` because `inventory::submit!` needs
        // an addressable function pointer; we route the visibility
        // through a documented `__djogi_*` prefix per the macro-
        // routing convention.
        #[doc(hidden)]
        pub fn #caster_fn_ident(
            _any: &::std::sync::Arc<dyn ::std::any::Any + ::core::marker::Send + ::core::marker::Sync>,
        ) -> ::core::option::Option<
            ::std::sync::Arc<dyn ::std::any::Any + ::core::marker::Send + ::core::marker::Sync>,
        > {
            // T5.3 fills this body. T5.2 ships a placeholder that
            // unconditionally returns `None` so the registration is
            // reachable via `iter_for_trait::<dyn T>` but the
            // consumer-side downcast is a no-op until T5.3 lands.
            ::core::option::Option::None
        }

        ::djogi::__private::inventory::submit! {
            ::djogi::trait_registry::TraitRegistration {
                model_type_id: || ::std::any::TypeId::of::<#self_ty>(),
                trait_type_id: || ::std::any::TypeId::of::<dyn #trait_path>(),
                model_type_name: #model_type_name,
                trait_type_name: #trait_type_name,
                caster: #caster_fn_ident,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bare-minimum trait impl parses + emits both the impl block
    /// and the registration. The emitted token stream contains the
    /// impl block verbatim so adopter-side compile errors point at
    /// the adopter's own code.
    #[test]
    fn parses_trait_impl_attribute() {
        let item: TokenStream = quote! {
            impl Searchable for Vehicle {
                fn searchable_columns(&self) -> &'static [&'static str] {
                    &["title"]
                }
            }
        };
        let out = expand(quote! {}, item).to_string();
        assert!(out.contains("impl Searchable for Vehicle"));
        assert!(out.contains("inventory :: submit"));
        assert!(out.contains("TraitRegistration"));
        assert!(out.contains("model_type_name : \"Vehicle\""));
        assert!(out.contains("trait_type_name : \"Searchable\""));
    }

    /// Inherent impls (no trait) rejected with an actionable
    /// diagnostic.
    #[test]
    fn rejects_inherent_impl() {
        let item: TokenStream = quote! {
            impl Vehicle {
                fn helper(&self) {}
            }
        };
        let out = expand(quote! {}, item).to_string();
        // The compile-error tokens render as `compile_error ! { ... }`.
        assert!(out.contains("compile_error"));
        assert!(out.contains("inherent impl"));
    }

    /// Generic impls rejected — v0.1.0 only handles concrete impls.
    #[test]
    fn rejects_generic_impl() {
        let item: TokenStream = quote! {
            impl<T> Searchable for Vec<T> {
                fn searchable_columns(&self) -> &'static [&'static str] {
                    &[]
                }
            }
        };
        let out = expand(quote! {}, item).to_string();
        assert!(out.contains("compile_error"));
        assert!(out.contains("generic"));
    }
}
