//! Injects `id`, `created_at`, `updated_at` as the first fields of the struct.
//! Also generates a `Default` impl for struct-update syntax (`..Post::default()`).
//!
//! # Field injection
//!
//! The `#[model]` attribute macro calls `inject::expand`, which prepends the
//! three framework-managed fields to the user's named field list. The fields
//! are typed according to the `pk` strategy:
//!
//! | pk        | `id` type                  |
//! |-----------|----------------------------|
//! | `heerid`  | `djogi::types::HeerId`     |
//! | `ranjid`  | `djogi::types::RanjId`     |
//! | `serial`  | `i32`                      |
//! | `none`    | (not injected)             |
//!
//! `created_at` and `updated_at` are always `djogi::types::DateTime` and always
//! injected — even for `pk = "none"`.
//!
//! Types are routed through `::djogi::types::*` rather than `::heeranjid::*` /
//! `::time::*` directly so that users only need `djogi` as a direct dependency.
//!
//! # Default impl
//!
//! The generated `Default` impl is designed for struct-update syntax:
//!
//! ```ignore
//! let draft = Post {
//!     title: "Hello".into(),
//!     ..Post::default()
//! };
//! Post::create(&pool, draft).await?;
//! ```
//!
//! The sentinel values (`HeerId(0)`, `UNIX_EPOCH`) are *never* written to the
//! database — `create()` uses `RETURNING *` to populate them from DB defaults.
//! They exist purely to satisfy Rust's requirement that every field in a struct
//! literal is initialised.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{ItemStruct, parse_quote};

use super::attrs::{ModelAttrs, PkStrategy};

/// Prepend framework fields to the struct and return the modified struct
/// definition plus a `Default` impl, concatenated into a single `TokenStream`.
///
/// This is the only entry point. Callers must pass a `mut` borrow because the
/// struct's field list is reordered in-place.
pub fn expand(struct_item: &mut ItemStruct, model_attrs: &ModelAttrs) -> TokenStream {
    inject_fields(struct_item, model_attrs);
    let default_impl = generate_default_impl(struct_item, model_attrs);

    quote! {
        #struct_item
        #default_impl
    }
}

/// Prepend `id`, `created_at`, `updated_at` to the struct's named field list.
///
/// For `pk = "none"` only the timestamps are injected — callers that need a
/// primary key must add it themselves as a regular user field.
///
/// Tuple structs and unit structs are left unchanged. A later error from
/// `crud::expand` / `from_row::expand` will surface the unsupported shape with
/// a clearer message than we can produce here.
///
/// # Why `::djogi::types::*` rather than `::heeranjid::*` / `::time::*`?
///
/// The macro emits code that runs in the user's crate. Users depend on `djogi`
/// but may not have `heeranjid` or `time` as direct crate-level dependencies.
/// Referencing those through `::djogi::types` avoids the E0433 "crate not found"
/// error — `djogi` re-exports `HeerId`, `RanjId`, `DateTime`, etc. via its
/// `types` module, so a single dependency is all the user ever needs.
fn inject_fields(struct_item: &mut ItemStruct, model_attrs: &ModelAttrs) {
    let id_field: Option<syn::Field> = match model_attrs.pk {
        PkStrategy::HeerId => Some(parse_quote! { pub id: ::djogi::types::HeerId }),
        PkStrategy::RanjId => Some(parse_quote! { pub id: ::djogi::types::RanjId }),
        PkStrategy::Serial => Some(parse_quote! { pub id: i32 }),
        PkStrategy::None => None,
    };

    let created_at_field: syn::Field = parse_quote! { pub created_at: ::djogi::types::DateTime };
    let updated_at_field: syn::Field = parse_quote! { pub updated_at: ::djogi::types::DateTime };

    if let syn::Fields::Named(named) = &mut struct_item.fields {
        let existing: Vec<_> = named.named.iter().cloned().collect();
        named.named.clear();
        if let Some(id) = id_field {
            named.named.push(id);
        }
        named.named.push(created_at_field);
        named.named.push(updated_at_field);
        for field in existing {
            named.named.push(field);
        }
    }
}

/// Generate `impl Default for <Struct>` with sentinel values for framework fields.
///
/// Sentinel values:
/// - `HeerId` → `::djogi::types::__heerid_default()` (HeerId(0), always valid)
/// - `RanjId` → `::djogi::types::__ranjid_default()` (minimum valid RanjId sentinel)
/// - `i32` (serial) → `0i32`
/// - `created_at` / `updated_at` → `::djogi::types::DateTime::UNIX_EPOCH`
/// - User fields → `Default::default()` (user types must implement `Default`)
fn generate_default_impl(struct_item: &ItemStruct, model_attrs: &ModelAttrs) -> TokenStream {
    let name = &struct_item.ident;
    let (impl_generics, ty_generics, where_clause) = struct_item.generics.split_for_impl();

    // Defaults for user fields — their types must implement Default.
    let user_field_defaults: Vec<TokenStream> = struct_item
        .fields
        .iter()
        .filter(|f| {
            let n = f.ident.as_ref().map(|i| i.to_string()).unwrap_or_default();
            !matches!(n.as_str(), "id" | "created_at" | "updated_at")
        })
        .map(|f| {
            let fname = &f.ident;
            quote! { #fname: ::std::default::Default::default() }
        })
        .collect();

    let id_part = match model_attrs.pk {
        PkStrategy::HeerId => quote! { id: ::djogi::types::__heerid_default(), },
        PkStrategy::RanjId => quote! { id: ::djogi::types::__ranjid_default(), },
        PkStrategy::Serial => quote! { id: 0i32, },
        PkStrategy::None => quote! {},
    };

    let timestamp_defaults = quote! {
        created_at: ::djogi::types::DateTime::UNIX_EPOCH,
        updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
    };

    quote! {
        impl #impl_generics ::std::default::Default for #name #ty_generics #where_clause {
            fn default() -> Self {
                #name {
                    #id_part
                    #timestamp_defaults
                    #(#user_field_defaults,)*
                }
            }
        }
    }
}
