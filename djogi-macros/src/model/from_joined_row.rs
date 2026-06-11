//! Generates `impl ::djogi::pg::decode::FromJoinedPgRow for T`.
//! # What
//! Emits the prefix-aware row decoder for every `#[model]`-annotated struct.
//! The generated `from_joined_pg_row(row, prefix)` reads each field by name via
//! `row.try_get(...)`, letting the
//! `select_related` emitter decode both the parent (empty prefix) and a
//! child (e.g. `"rel_owner_id."`) from the same joined row without
//! column-name collisions.
//! # Why a sibling impl to `FromPgRow`
//! [`FromPgRow`](::djogi::pg::decode::FromPgRow) decodes by canonical
//! projection order and therefore has no prefix parameter. Joined decode
//! needs a caller-supplied alias stem, so the macro emits a sibling impl
//! with one `row.try_get` per field using stable alias mapping:
//! `"{prefix}{column_name}"` for generic joined rows and `o{idx}` / `n{idx}`
//! for legacy `__djogi_old__` / `__djogi_new__` decoding.
//! An empty prefix (`""`) degenerates to the same column names the model
//! declares directly. The macro intentionally does not derive joined decode
//! through `FromPgRow`: one path is positional, the other is name-based and
//! prefix-aware.
//! # How
//! Column name == field name, same convention the `from_row::expand`
//! uses. Injected framework fields (`id` / `created_at` / `updated_at`) are
//! included automatically because the macro iterates the post-injection
//! struct shape.
//! # Where
//! Called from `mod.rs` after `inject::expand` has mutated the struct, so the
//! iterator includes the framework fields without extra bookkeeping.

use proc_macro2::TokenStream;
use quote::quote;
use syn::ItemStruct;

use super::attrs::{FieldAttrs, ModelAttrs, PkStrategy};
use super::sql_bind::{bind_kind, decode_joined_field_tokens, is_nullable, is_tracked_inner};

/// Generate the `FromJoinedPgRow` impl for `struct_item`.
/// `model_attrs` provides pk strategy info for framework field count.
/// `field_attrs` carries per-field metadata including codec information
/// for protected fields with encryption.
pub fn expand(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    field_attrs: &[FieldAttrs],
) -> TokenStream {
    let name = &struct_item.ident;
    let (impl_generics, ty_generics, where_clause) = struct_item.generics.split_for_impl();

    // Build a codec vector aligned with ALL struct fields (None for framework,
    // Some/None for users). `field_attrs` is user-only but `struct_item.fields`
    // includes framework fields at the front, so we can't zip them directly.
    let n_framework = match model_attrs.pk {
        PkStrategy::None => 2, // created_at, updated_at only
        _ => 3,                // id, created_at, updated_at
    };
    let codec_vec: Vec<Option<(String, String, String)>> = struct_item
        .fields
        .iter()
        .enumerate()
        .map(|(idx, field)| {
            if idx < n_framework {
                None // Framework fields have no codec.
            } else {
                let fa_idx = idx - n_framework;
                field_attrs
                    .get(fa_idx)
                    .and_then(|fa| fa.protected.as_ref().and_then(|p| p.codec.clone()))
                    .map(|codec_id| {
                        let col_name = crate::syn_util::column_name_from_field(field);
                        (codec_id, name.to_string(), col_name)
                    })
            }
        })
        .collect();

    // One prefix-aware decode per field. For direct types, uses
    // `row.try_get::<_, _>` (same as before). For widened types
    // (i8/u8/u16/u32/u64), delegates to the appropriate
    // `decode_narrowed_by_name` / `decode_u64_from_decimal_by_name` variant
    // so the narrowing conversion is applied after the wide-type read.
    // For protected fields, reads Vec<u8> by name and calls codec decode.
    let field_assignments: Vec<TokenStream> = struct_item
        .fields
        .iter()
        .enumerate()
        .map(|(idx, f)| {
            let fname = f.ident.as_ref().expect("only named structs supported");
            // Raw identifiers (`r#type`) must strip the `r#` prefix to match
            // the SQL column name — same rule as `from_row::expand`.
            let col_name = crate::syn_util::column_name_from_field(f);
            let kind = bind_kind(&f.ty);
            let nullable = is_nullable(&f.ty);
            let tracked = is_tracked_inner(&f.ty);
            let codec = codec_vec[idx].clone();
            let col_name_expr = quote! {
                &::djogi::__private::pg::joined_alias_for_prefix(
                    prefix,
                    #idx,
                    #col_name,
                ) as &str
            };
            let decode_expr =
                decode_joined_field_tokens(&kind, nullable, tracked, col_name_expr, codec);
            quote! {
                #fname: #decode_expr
            }
        })
        .collect();

    let debug_alias_guards: Vec<TokenStream> = struct_item
        .fields
        .iter()
        .enumerate()
        .map(|(idx, f)| {
            let col_name = crate::syn_util::column_name_from_field(f);
            quote! {
                {
                    let __expected_alias = ::djogi::__private::pg::joined_alias_for_prefix(
                        prefix,
                        #idx,
                        #col_name,
                    );
                    assert!(
                        __djogi_joined_columns.iter().any(|__column| __column.name() == __expected_alias),
                        "FromJoinedPgRow alias drift: prefix {:?} missing alias {:?} for field {:?} at position {}",
                        prefix,
                        __expected_alias,
                        #col_name,
                        #idx,
                    );
                }
            }
        })
        .collect();

    quote! {
        impl #impl_generics ::djogi::pg::decode::FromJoinedPgRow for #name #ty_generics #where_clause {
            fn from_joined_pg_row(
                row: &::djogi::__private::tokio_postgres::Row,
                prefix: &str,
            ) -> ::std::result::Result<Self, ::djogi::DjogiError> {
                #[cfg(debug_assertions)]
                {
                    let __djogi_joined_columns = row.columns();
                    #(#debug_alias_guards)*
                }
                ::std::result::Result::Ok(Self {
                    #(#field_assignments,)*
                })
            }
        }
    }
}
