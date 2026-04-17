//! Generates `impl sqlx::FromRow<'_, sqlx::postgres::PgRow> for T`.
//!
//! # What
//! Emits a `FromRow` implementation for every `#[model]`-annotated struct so
//! that SQLx can deserialize Postgres query results directly into the user's
//! type — including the three framework-injected fields (`id`, `created_at`,
//! `updated_at`).
//!
//! # Why
//! SQLx's own `#[derive(FromRow)]` cannot see the injected fields because they
//! are added by our proc macro *after* the derive attribute is processed. We
//! therefore generate the impl ourselves, using the struct shape as it exists
//! *after* injection.
//!
//! # How
//! Column name == field name by convention (snake_case). This is correct for
//! Phase 1 because the migration generator (Task 6+) uses the same convention.
//! Future tasks may extend this to respect a `#[field(column = "…")]` override.
//!
//! # Where
//! Called from `mod.rs` after `inject::expand` has mutated the struct, so the
//! iterator includes `id`, `created_at`, and `updated_at` automatically.

use proc_macro2::TokenStream;
use quote::quote;
use syn::ItemStruct;

use super::attrs::{FieldAttrs, ModelAttrs};

/// Generate the `FromRow` impl for `struct_item`.
///
/// `model_attrs` and `field_attrs` are accepted for API consistency with other
/// `expand` functions and for future use (e.g. `column` overrides).
pub fn expand(
    struct_item: &ItemStruct,
    _model_attrs: &ModelAttrs,
    _field_attrs: &[FieldAttrs],
) -> TokenStream {
    let name = &struct_item.ident;
    let (impl_generics, ty_generics, where_clause) = struct_item.generics.split_for_impl();

    // Column name == field name. Injected framework fields (id / created_at /
    // updated_at) follow the same convention — no special-casing needed.
    let field_assignments: Vec<TokenStream> = struct_item
        .fields
        .iter()
        .map(|f| {
            let fname = f.ident.as_ref().expect("only named structs supported");
            let col_name = fname.to_string();
            quote! {
                #fname: row.try_get(#col_name)?
            }
        })
        .collect();

    // Use `::djogi::__private::sqlx` rather than bare `::sqlx` so that
    // macro-generated code compiles in any crate that depends on `djogi` —
    // users should not need `sqlx` as a direct dependency just to use `#[model]`.
    quote! {
        impl #impl_generics ::djogi::__private::sqlx::FromRow<
            '_,
            ::djogi::__private::sqlx::postgres::PgRow,
        > for #name #ty_generics #where_clause
        {
            fn from_row(
                row: &::djogi::__private::sqlx::postgres::PgRow,
            ) -> ::djogi::__private::sqlx::Result<Self> {
                use ::djogi::__private::sqlx::Row;
                Ok(Self {
                    #(#field_assignments,)*
                })
            }
        }
    }
}
