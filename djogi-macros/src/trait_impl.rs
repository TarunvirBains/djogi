//! `#[djogi::trait_impl]` attribute macro.
//! Wraps a trait `impl` block with a sibling `inventory::submit!`
//! registration so cross-cutting consumers (`Sassi::all_impl::<dyn T>`)
//! can iterate every model that implements a given
//! trait without naming each model in the consumer's path.
//! Emits the impl block verbatim plus a sibling `inventory::submit!`
//! `TraitRegistration` and a type-erased caster that uses the safe
//! `Arc<dyn Any>` → `Arc<Self>` → `Arc<dyn Trait>` carrier pattern
//! (no `transmute`) so consumers can downcast registry entries back to
//! the concrete trait object.
//! # Why a separate attribute macro
//! Adopters writing `impl Searchable for Vehicle {... }` register
//! with one attribute prefix:
//! ```ignore
//! #[djogi::trait_impl]
//! impl Searchable for Vehicle {
//!  fn searchable_columns(&self) -> &[&'static str] { &["title"] }
//! }
//! ```
//! No additional code at the impl block; the macro emits a
//! `inventory::submit!(TraitRegistration {... })` alongside the
//! unchanged impl block. The impl block reaches rustc verbatim so
//! adopter-side compile errors (e.g. wrong method signature) point
//! at the adopter's own code, not at the macro expansion.
//! # What we accept
//! - Trait impls only — `impl Trait for Type {... }`. Inherent
//! `impl Type {... }` rejected.
//! - Concrete (non-generic) impls only — `impl<T> Trait for Vec<T>`
//! rejected. Generic impls would require parameter substitution
//! for the `TypeId::of` lookup at registration time, which is
//! deferred to a future phase per `feedback_anchored_deferrals`.
//! - Single-segment `Self` types — `impl Trait for Vehicle` works,
//! `impl Trait for crate::module::Vehicle` works (path resolved
//! verbatim), `impl Trait for some_fn()::Vehicle` rejected (not a
//! nameable type at parse time).

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
    inherent impl blocks (`impl Type {... }`) cannot be \
    registered for cross-type queries; rewrite as \
    `impl SomeTrait for Type {... }`",
        )
    })?;
    // syn::ItemImpl::trait_ is `Option<(Option<!>, Path, Token![for])>`.
    let trait_path = &trait_path_pair.1;

    // Reject generic impls (`impl<T> Trait for Vec<T>`). Only
    // non-generic concrete impls are handled in v0.1.0; generic impls
    // would require runtime parameter substitution for the
    // `TypeId::of` lookup which is deferred to a future phase per
    // `feedback_anchored_deferrals`.
    if !item_impl.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &item_impl.generics,
            "`#[djogi::trait_impl]` does not yet support generic impls \
    — concrete `impl Trait for ConcreteType {... }` only in \
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

    // Safe type-erased caster body — avoid `transmute` entirely:
    // the path is
    // 1. `Arc<dyn Any + Send + Sync>` → `Arc<Self>` via `Arc::downcast`
    // 2. `Arc<Self>` → `Arc<dyn Trait>` via unsizing coercion
    // 3. Box the `Arc<dyn Trait>` in a per-(Model, Trait) carrier
    // struct that itself is `Send + Sync + 'static` (so it
    // satisfies `Any + Send + Sync`).
    // 4. Erase the carrier back to `Arc<dyn Any + Send + Sync>` for
    // the registry's wire type.
    // The consuming side (`Sassi::all_impl::<dyn T>`)
    // performs the symmetric downcast: `Arc<dyn Any>` →
    // `Arc<TraitImplCarrier<dyn T>>` via `Arc::downcast`, then
    // unwraps the inner `Arc<dyn T>` from the carrier's `into_arc`
    // method. No `transmute` at any point in the chain.
    // The carrier struct is emitted per impl site so multiple
    // `#[djogi::trait_impl]` blocks in the same module do not
    // collide on a shared type name. The `__djogi_trait_obj_*`
    // prefix mirrors the caster fn's naming convention.
    let carrier_struct_ident = format_ident!(
        "__djogi_trait_obj_{}_{}",
        model_type_name.replace("::", "_"),
        trait_last_seg,
    );

    Ok(quote! {
     #item_impl

     // Per-impl carrier struct — wraps the unsized `Arc<dyn Trait>`
     // in a sized `Send + Sync + 'static` shell so it satisfies
     // the `Any` bound the registry's wire type requires. The
     // inner field is `pub` so the consuming side can extract
     // the Arc through the `into_arc` accessor.
     #[doc(hidden)]
     pub struct #carrier_struct_ident(
      pub ::std::sync::Arc<dyn #trait_path + ::core::marker::Send + ::core::marker::Sync>,
     );

     impl #carrier_struct_ident {
      /// Extract the underlying `Arc<dyn Trait>` from the
      /// carrier. The consuming side calls this after the
      /// `Arc<dyn Any>` → `Arc<#carrier>` downcast succeeds.
      #[doc(hidden)]
      pub fn into_arc(
       self,
      ) -> ::std::sync::Arc<dyn #trait_path + ::core::marker::Send + ::core::marker::Sync>
      {
       self.0
      }
     }

     // Type-erased caster — safe carrier pattern; no `transmute`.
     // Returns `Some(arc_to_carrier)` when the input downcasts to
     // the registered model type; `None` otherwise.
     #[doc(hidden)]
     pub fn #caster_fn_ident(
      any: &::std::sync::Arc<dyn ::std::any::Any + ::core::marker::Send + ::core::marker::Sync>,
     ) -> ::core::option::Option<
      ::std::sync::Arc<dyn ::std::any::Any + ::core::marker::Send + ::core::marker::Sync>,
     > {
      // Step 1 — downcast the erased Arc to `Arc<Self>`.
      // `Arc::downcast` on `Arc<dyn Any + Send + Sync>` returns
      // `Result<Arc<T>, Arc<dyn Any + Send + Sync>>` when `T:
      // Any + Send + Sync` — we discard the `Err` arm via `.ok()`.
      let arc_model: ::std::sync::Arc<#self_ty> =
       match ::std::sync::Arc::clone(any).downcast::<#self_ty>() {
        ::core::result::Result::Ok(arc) => arc,
        ::core::result::Result::Err(_) => return ::core::option::Option::None,
       };

      // Step 2 — unsizing coercion: `Arc<Self>` → `Arc<dyn Trait + Send + Sync>`.
      // The coercion is sound because `Self: Trait` (the
      // surrounding `impl Trait for Self` block proves it) and
      // `Self: Send + Sync + 'static` (every model's struct
      // injection guarantees these bounds).
      let arc_trait: ::std::sync::Arc<dyn #trait_path + ::core::marker::Send + ::core::marker::Sync> =
       arc_model;

      // Step 3 — wrap in the per-impl carrier so the result is
      // `Sized + Any + Send + Sync`.
      let carrier = #carrier_struct_ident(arc_trait);
      let arc_carrier: ::std::sync::Arc<#carrier_struct_ident> =
       ::std::sync::Arc::new(carrier);

      // Step 4 — erase the carrier to `Arc<dyn Any + Send + Sync>`
      // for the registry wire type. This is a coercion (`Arc<T>`
      // → `Arc<dyn Any>` for `T: Any`), not a transmute.
      ::core::option::Option::Some(arc_carrier)
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
        // The compile-error tokens render as `compile_error ! {... }`.
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
