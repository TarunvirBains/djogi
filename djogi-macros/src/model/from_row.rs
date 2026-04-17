//! Generates `impl sqlx::FromRow<'_, sqlx::postgres::PgRow> for T` so the ORM
//! layer can deserialise query results into user structs. Implemented in Task 5.

use proc_macro2::TokenStream;
use syn::ItemStruct;

use super::attrs::ModelAttrs;

/// Stub — returns an empty token stream. Task 5 replaces this.
pub fn expand(_struct_item: &ItemStruct, _model_attrs: &ModelAttrs) -> TokenStream {
    quote::quote! {}
}
