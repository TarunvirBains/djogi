//! Generates a `ModelDescriptor` constant and an `inventory::submit!` call so
//! the app registry can discover every model at startup without manual wiring.
//! Implemented in Task 6.

use proc_macro2::TokenStream;
use syn::ItemStruct;

use super::attrs::{FieldAttrs, ModelAttrs};

/// Stub — returns an empty token stream. Task 6 replaces this.
pub fn expand(
    _struct_item: &ItemStruct,
    _model_attrs: &ModelAttrs,
    _field_attrs: &[FieldAttrs],
) -> TokenStream {
    quote::quote! {}
}
