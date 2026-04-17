//! Generates `impl djogi::Model for T` — the CRUD surface: `create`, `save`,
//! `delete`, `find`, `filter`, and `QuerySet` wiring. Implemented in Tasks 7–9.

use proc_macro2::TokenStream;
use syn::ItemStruct;

use super::attrs::{FieldAttrs, ModelAttrs};

/// Stub — returns an empty token stream. Tasks 7–9 replace this.
pub fn expand(
    _struct_item: &ItemStruct,
    _model_attrs: &ModelAttrs,
    _field_attrs: &[FieldAttrs],
) -> TokenStream {
    quote::quote! {}
}
