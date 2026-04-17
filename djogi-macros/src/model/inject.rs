//! Injects `id`, `created_at`, `updated_at` into the struct and generates a
//! `Default` impl that zeroes those fields. Implemented in Task 4.

use proc_macro2::TokenStream;
use syn::ItemStruct;

use super::attrs::ModelAttrs;

/// Stub — returns the struct unchanged and an empty `Default` impl.
/// Task 4 replaces this with real field injection.
pub fn expand(
    _struct_item: &mut ItemStruct,
    _model_attrs: &ModelAttrs,
) -> (TokenStream, TokenStream) {
    (quote::quote! {}, quote::quote! {})
}
