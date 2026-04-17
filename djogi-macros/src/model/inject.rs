//! Injects `id`, `created_at`, `updated_at` into the struct and generates a
//! `Default` impl that zeroes those fields. Implemented in Task 4.
//!
//! The Task 3 stub preserves the user's struct verbatim so `#[model]`
//! applied in Tasks 3–4 does not erase the annotated type — the macro must
//! always emit at least the original struct definition, or user code fails
//! to compile with "cannot find type `Foo`".

use proc_macro2::TokenStream;
use quote::quote;
use syn::ItemStruct;

use super::attrs::ModelAttrs;

/// Stub — returns the struct verbatim and an empty `Default` impl.
/// Task 4 replaces this with real field injection.
pub fn expand(
    struct_item: &mut ItemStruct,
    _model_attrs: &ModelAttrs,
) -> (TokenStream, TokenStream) {
    (quote! { #struct_item }, TokenStream::new())
}
