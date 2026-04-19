//! Generates `impl ::djogi::relation::FromJoinedRow for T`.
//!
//! # What
//!
//! Emits the prefix-aware row decoder for every `#[model]`-annotated struct.
//! The generated `from_prefixed_row(row, prefix)` reads each field under
//! `"{prefix}{column_name}"` via `row.try_get(...)`, letting the
//! `select_related` emitter decode both the parent (empty prefix) and a
//! child (e.g. `"rel_owner_id."`) from the same joined row without
//! column-name collisions.
//!
//! # Why a sibling impl to `FromRow`
//!
//! sqlx's [`FromRow`](sqlx::FromRow) looks columns up by bare name — there is
//! no "prefix" knob to thread through, and intercepting every lookup with a
//! newtype wrapper around `PgRow` would be both slower (indirection per
//! column) and more invasive than the sibling impl this module emits. The
//! implementation shape is otherwise identical to `from_row::expand`'s
//! emission: one `row.try_get` per field, column name == field name by
//! convention (snake_case).
//!
//! An empty prefix (`""`) degenerates to the same lookups `FromRow` would
//! perform — passing `""` to `from_prefixed_row` is operationally equivalent
//! to calling `T::from_row(&row)`. The macro intentionally does NOT blanket-
//! impl `FromJoinedRow` via `FromRow` because the two live on different
//! tree-shaking paths: a user who never touches `select_related` never pays
//! the extra monomorphisation of `from_prefixed_row`, and vice versa.
//!
//! # How
//!
//! Column name == field name, same convention Phase 1's `from_row::expand`
//! uses. Injected framework fields (`id` / `created_at` / `updated_at`) are
//! included automatically because the macro iterates the post-injection
//! struct shape.
//!
//! # Where
//!
//! Called from `mod.rs` after `inject::expand` has mutated the struct, so the
//! iterator includes the framework fields without extra bookkeeping.

use proc_macro2::TokenStream;
use quote::quote;
use syn::ItemStruct;

use super::attrs::{FieldAttrs, ModelAttrs};

/// Generate the `FromJoinedRow` impl for `struct_item`.
///
/// `model_attrs` and `field_attrs` are accepted for API consistency with the
/// sibling `from_row::expand` and for future use (e.g. `column` overrides).
pub fn expand(
    struct_item: &ItemStruct,
    _model_attrs: &ModelAttrs,
    _field_attrs: &[FieldAttrs],
) -> TokenStream {
    let name = &struct_item.ident;
    let (impl_generics, ty_generics, where_clause) = struct_item.generics.split_for_impl();

    // One prefix-aware `try_get` per field. `format!` happens once per call —
    // the overhead is a small String allocation per field per decoded row,
    // swamped by the sqlx decode cost itself.
    let field_assignments: Vec<TokenStream> = struct_item
        .fields
        .iter()
        .map(|f| {
            let fname = f.ident.as_ref().expect("only named structs supported");
            // Raw identifiers (`r#type`) must strip the `r#` prefix to match
            // the SQL column name — same rule as `from_row::expand`.
            let raw_name = fname.to_string();
            let col_name = raw_name.strip_prefix("r#").unwrap_or(&raw_name).to_string();
            quote! {
                #fname: row.try_get::<_, &str>(&::std::format!("{}{}", prefix, #col_name))?
            }
        })
        .collect();

    quote! {
        impl #impl_generics ::djogi::relation::FromJoinedRow for #name #ty_generics #where_clause {
            fn from_prefixed_row(
                row: &::djogi::__private::sqlx::postgres::PgRow,
                prefix: &str,
            ) -> ::djogi::__private::sqlx::Result<Self> {
                use ::djogi::__private::sqlx::Row;
                Ok(Self {
                    #(#field_assignments,)*
                })
            }
        }
    }
}
