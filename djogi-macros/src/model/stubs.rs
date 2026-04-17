//! Generates `{Model}Fields` (typed field-accessor constants for closure-based
//! filters) and `{Model}Filter` (programmatic filter builder for shell/dynamic
//! use). Implemented in Task 6.

use proc_macro2::TokenStream;
use syn::ItemStruct;

/// Stub — returns an empty token stream. Task 6 replaces this.
pub fn expand(_struct_item: &ItemStruct) -> TokenStream {
    quote::quote! {}
}
