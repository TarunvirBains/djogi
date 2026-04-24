//! Injects `id`, `created_at`, `updated_at` as the first fields of the struct.
//! Also generates a `Default` impl for struct-update syntax (`..Post::default()`).
//!
//! # Field injection
//!
//! The `#[model]` attribute macro calls `inject::expand`, which prepends the
//! framework-managed fields to the user's named field list. The `id` field is
//! typed according to the `pk` strategy:
//!
//! | pk        | `id` type                  |
//! |-----------|----------------------------|
//! | `heerid`  | `djogi::types::HeerId`     |
//! | `ranjid`  | `djogi::types::RanjId`     |
//! | `serial`  | `i32`                      |
//! | `none`    | (not injected — user supplies their own PK field) |
//!
//! `created_at` and `updated_at` are always `djogi::types::DateTime` and always
//! injected — regardless of `pk` strategy.
//!
//! Types are routed through `::djogi::types::*` rather than `::heeranjid::*` /
//! `::time::*` directly so that users only need `djogi` as a direct dependency.
//!
//! # Validation
//!
//! `expand` returns `syn::Result` so it can emit targeted compile errors instead
//! of letting Rust's duplicate-field / unsupported-shape messages surface:
//!
//! - **Tuple / unit structs** are rejected with `#[model] requires a struct with
//!   named fields` at the struct's ident.
//! - **Reserved names.** A user field named `created_at` or `updated_at` is
//!   rejected unconditionally (the macro always injects those). A user field
//!   named `id` is rejected for every `pk` strategy except `"none"` — under
//!   `pk = "none"` the user is *expected* to declare their own `id` (or other
//!   PK-carrying field) and the filter below preserves it.
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
use syn::{ItemStruct, parse_quote, spanned::Spanned};

use super::attrs::{ModelAttrs, PkStrategy};

/// Reserved field names that the macro injects and users cannot redefine.
/// `id` is conditionally reserved — see `reserved_for_pk`.
const ALWAYS_RESERVED: &[&str] = &["created_at", "updated_at"];

/// Prepend framework fields to the struct and return the modified struct
/// definition plus a `Default` impl, concatenated into a single `TokenStream`.
///
/// Returns `syn::Error` if:
/// - the struct is not `Fields::Named` (tuple / unit shape), or
/// - the user declared a reserved field name (`created_at` / `updated_at`
///   always; `id` except under `pk = "none"`).
///
/// When `model_attrs.no_default` is `true`, the `Default` impl is omitted.
/// This is required for models that contain field types that do not implement
/// `Default` (e.g. `time::Date`). Those models cannot use struct-update
/// syntax (`..Model::default()`) — all fields must be initialised explicitly.
///
/// Callers must pass a `mut` borrow because the struct's field list is
/// reordered in-place.
pub fn expand(struct_item: &mut ItemStruct, model_attrs: &ModelAttrs) -> syn::Result<TokenStream> {
    validate_shape(struct_item)?;
    validate_field_names(struct_item, model_attrs)?;

    inject_fields(struct_item, model_attrs);

    if model_attrs.no_default {
        Ok(quote! { #struct_item })
    } else {
        let default_impl = generate_default_impl(struct_item, model_attrs);
        Ok(quote! {
            #struct_item
            #default_impl
        })
    }
}

/// Reject tuple / unit structs up front so downstream modules see only named
/// structs. The error points at the struct's identifier for a clean span.
fn validate_shape(struct_item: &ItemStruct) -> syn::Result<()> {
    if matches!(struct_item.fields, syn::Fields::Named(_)) {
        Ok(())
    } else {
        Err(syn::Error::new(
            struct_item.ident.span(),
            "#[model] requires a struct with named fields — tuple and unit structs are not supported",
        ))
    }
}

/// Reject user fields whose names collide with framework-injected fields.
///
/// `created_at` / `updated_at` are always reserved. `id` is reserved except
/// for `pk = "none"`, where the user is expected to declare their own PK
/// field (which may or may not be called `id`).
fn validate_field_names(struct_item: &ItemStruct, model_attrs: &ModelAttrs) -> syn::Result<()> {
    let id_is_reserved = reserved_for_pk(model_attrs);
    if let syn::Fields::Named(named) = &struct_item.fields {
        for field in &named.named {
            let Some(ident) = &field.ident else { continue };
            let name = ident.to_string();
            let is_reserved =
                ALWAYS_RESERVED.contains(&name.as_str()) || (id_is_reserved && name == "id");
            if is_reserved {
                return Err(syn::Error::new(
                    field.span(),
                    format!(
                        "#[model] reserves the field name `{name}` — the macro injects it automatically. \
                         Rename your field or, if you need to control the primary key yourself, set `#[model(pk = \"none\")]`."
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// `true` when the current `pk` strategy injects an `id` field, making `id`
/// reserved. Only `pk = "none"` lets users declare their own `id`.
fn reserved_for_pk(model_attrs: &ModelAttrs) -> bool {
    !matches!(model_attrs.pk, PkStrategy::None)
}

/// Prepend framework fields in front of the user's named fields.
/// Assumes `validate_shape` + `validate_field_names` already succeeded.
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
        PkStrategy::HeerIdDesc => Some(parse_quote! { pub id: ::djogi::types::HeerIdDesc }),
        PkStrategy::RanjIdDesc => Some(parse_quote! { pub id: ::djogi::types::RanjIdDesc }),
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
/// - `HeerId` / `HeerIdDesc` / `RanjId` / `RanjIdDesc` →
///   `<T as ::djogi::primary_key::PrimaryKey>::sentinel()` — zero-valued
///   instance the trait factory produces. Replaces the pre-Phase-7-Zero-2
///   `::djogi::types::__*_default()` hidden helpers.
/// - `i32` (serial) → `0i32` (matches `<i32 as PrimaryKey>::sentinel()`)
/// - `created_at` / `updated_at` → `::djogi::types::DateTime::UNIX_EPOCH`
/// - User fields → `Default::default()` (user types must implement `Default`)
///
/// The `user_field_defaults` filter operates on the struct's field list
/// *after* `inject_fields` has prepended framework fields. For `pk = "none"`,
/// no `id` is injected, so a user's own `id` field (if present) survives the
/// filter and gets a `Default::default()` entry like any other user field.
fn generate_default_impl(struct_item: &ItemStruct, model_attrs: &ModelAttrs) -> TokenStream {
    let name = &struct_item.ident;
    let (impl_generics, ty_generics, where_clause) = struct_item.generics.split_for_impl();

    // Names that were actually injected (and therefore must be excluded from
    // the user-field default loop because they're initialised explicitly below).
    let skip_id = !matches!(model_attrs.pk, PkStrategy::None);

    let user_field_defaults: Vec<TokenStream> = struct_item
        .fields
        .iter()
        .filter(|f| {
            let n = f.ident.as_ref().map(|i| i.to_string()).unwrap_or_default();
            match n.as_str() {
                "created_at" | "updated_at" => false,
                "id" if skip_id => false,
                _ => true,
            }
        })
        .map(|f| {
            let fname = &f.ident;
            quote! { #fname: ::std::default::Default::default() }
        })
        .collect();

    let id_part = match model_attrs.pk {
        PkStrategy::HeerId => quote! {
            id: <::djogi::types::HeerId as ::djogi::primary_key::PrimaryKey>::sentinel(),
        },
        PkStrategy::RanjId => quote! {
            id: <::djogi::types::RanjId as ::djogi::primary_key::PrimaryKey>::sentinel(),
        },
        PkStrategy::HeerIdDesc => quote! {
            id: <::djogi::types::HeerIdDesc as ::djogi::primary_key::PrimaryKey>::sentinel(),
        },
        PkStrategy::RanjIdDesc => quote! {
            id: <::djogi::types::RanjIdDesc as ::djogi::primary_key::PrimaryKey>::sentinel(),
        },
        PkStrategy::Serial => quote! {
            id: <i32 as ::djogi::primary_key::PrimaryKey>::sentinel(),
        },
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
