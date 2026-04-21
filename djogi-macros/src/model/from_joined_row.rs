//! Generates `impl ::djogi::pg::decode::FromJoinedPgRow for T`.
//!
//! # What
//!
//! Emits the prefix-aware row decoder for every `#[model]`-annotated struct.
//! The generated `from_joined_pg_row(row, prefix)` reads each field under
//! `"{prefix}{column_name}"` via `row.try_get(...)`, letting the
//! `select_related` emitter decode both the parent (empty prefix) and a
//! child (e.g. `"rel_owner_id."`) from the same joined row without
//! column-name collisions.
//!
//! # Why a sibling impl to `FromPgRow`
//!
//! [`FromPgRow`](::djogi::pg::decode::FromPgRow) decodes by canonical
//! projection order and therefore has no prefix parameter. Joined decode
//! needs a caller-supplied alias stem, so the macro emits a sibling impl
//! with one `row.try_get` per field under `"{prefix}{column_name}"`.
//!
//! An empty prefix (`""`) degenerates to the same column names the model
//! declares directly. The macro intentionally does not derive joined decode
//! through `FromPgRow`: one path is positional, the other is name-based and
//! prefix-aware.
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

/// Generate the `FromJoinedPgRow` impl for `struct_item`.
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
    // swamped by the DB decode cost itself. The tokio-postgres `try_get`
    // signature is `try_get<'a, I, T>(&'a self, idx: I)` — we fix `I = &str`
    // (the column name) and let Rust infer `T` from the field's declared type.
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
                #fname: row.try_get::<_, _>(&::std::format!("{}{}", prefix, #col_name) as &str)?
            }
        })
        .collect();

    quote! {
        impl #impl_generics ::djogi::pg::decode::FromJoinedPgRow for #name #ty_generics #where_clause {
            fn from_joined_pg_row(
                row: &::djogi::__private::tokio_postgres::Row,
                prefix: &str,
            ) -> ::std::result::Result<Self, ::djogi::DjogiError> {
                ::std::result::Result::Ok(Self {
                    #(#field_assignments,)*
                })
            }
        }
    }
}
