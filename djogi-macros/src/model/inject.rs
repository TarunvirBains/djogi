//! Injects `id`, `created_at`, `updated_at` as the first fields of the struct.
//! Also generates a `Default` impl for struct-update syntax (`..Post::default`).
//! # Field injection
//! The `#[model]` attribute macro calls `inject::expand`, which prepends the
//! framework-managed fields to the user's named field list. The `id` field is
//! typed according to the `pk` strategy:
//! | pk | `id` type |
//! |-----------|----------------------------|
//! | `heerid` | `djogi::types::HeerId` |
//! | `ranjid` | `djogi::types::RanjId` |
//! | `serial` | `i32` |
//! | `none` | (not injected — user supplies their own PK field) |
//! `created_at` and `updated_at` are always `djogi::types::DateTime` and always
//! injected — regardless of `pk` strategy.
//! Types are routed through `::djogi::types::*` rather than `::heeranjid::*` /
//! `::time::*` directly so that users only need `djogi` as a direct dependency.
//! # Validation
//! `expand` returns `syn::Result` so it can emit targeted compile errors instead
//! of letting Rust's duplicate-field / unsupported-shape messages surface:
//! - **Tuple / unit structs** are rejected with `#[model] requires a struct with
//! named fields` at the struct's ident.
//! - **Reserved names.** A user field named `created_at` or `updated_at` is
//! rejected unconditionally (the macro always injects those). A user field
//! named `id` is rejected for every `pk` strategy except `"none"` — under
//! `pk = None` the user is *expected* to declare their own `id` (or other
//! PK-carrying field) and the filter below preserves it.
//! # Default impl
//! The generated `Default` impl is designed for struct-update syntax:
//! ```ignore
//! let draft = Post {
//!     title: "Hello".into(),
//!     ..Post::default()
//! };
//! Post::create(&pool, draft).await?;
//! ```
//! The sentinel values (`HeerId(0)`, `UNIX_EPOCH`) are *never* written to the
//! database — `create` uses `RETURNING *` to populate them from DB defaults.
//! They exist purely to satisfy Rust's requirement that every field in a struct
//! literal is initialised.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{ItemStruct, parse_quote, spanned::Spanned};

use super::attrs::{ModelAttrs, PkStrategy};

/// Framework field names that always exist post-injection: `created_at`
/// and `updated_at`. The `id` field is added on top of this list when the
/// PK strategy injects one — see [`is_framework_column`].
const ALWAYS_RESERVED: &[&str] = &["created_at", "updated_at"];

/// `true` when `name` denotes a field that the macro injects under the
/// current `model_attrs.pk` strategy.
/// Used by [`validate_field_names`] (to reject user fields whose names
/// collide with framework columns) and by [`generate_default_impl`] (to
/// skip framework columns in the user-field default loop, since they are
/// initialised explicitly by the surrounding `id` / timestamp blocks).
fn is_framework_column(name: &str, model_attrs: &ModelAttrs) -> bool {
    ALWAYS_RESERVED.contains(&name) || (reserved_for_pk(model_attrs) && name == "id")
}

/// Prepend framework fields to the struct and return the modified struct
/// definition plus a `Default` impl, concatenated into a single `TokenStream`.
/// Returns `syn::Error` if:
/// - the struct is not `Fields::Named` (tuple / unit shape), or
/// - the user declared a reserved field name (`created_at` / `updated_at`
/// always; `id` except under `pk = None`).
/// When `model_attrs.no_default` is `true`, the `Default` impl is omitted.
/// This is required for models that contain field types that do not implement
/// `Default` (e.g. `time::Date`). Those models cannot use struct-update
/// syntax (`..Model::default`) — all fields must be initialised explicitly.
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
/// `created_at` / `updated_at` are always reserved. `id` is reserved except
/// for `pk = None`, where the user is expected to declare their own PK
/// field (which may or may not be called `id`).
fn validate_field_names(struct_item: &ItemStruct, model_attrs: &ModelAttrs) -> syn::Result<()> {
    if let syn::Fields::Named(named) = &struct_item.fields {
        for field in &named.named {
            let Some(ident) = &field.ident else { continue };
            let name = ident.to_string();
            if is_framework_column(&name, model_attrs) {
                return Err(syn::Error::new(
                    field.span(),
                    format!(
                        "#[model] reserves the field name `{name}` — the macro injects it automatically. \
                         Rename your field or, if you need to control the primary key yourself, set `#[model(pk = None)]`."
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// `true` when the current `pk` strategy injects an `id` field, making `id`
/// reserved. Only `pk = None` lets users declare their own `id`.
fn reserved_for_pk(model_attrs: &ModelAttrs) -> bool {
    !matches!(model_attrs.pk, PkStrategy::None)
}

/// Prepend framework fields in front of the user's named fields.
/// Assumes `validate_shape` + `validate_field_names` already succeeded.
/// # Why `::djogi::types::*` rather than `::heeranjid::*` / `::time::*`?
/// The macro emits code that runs in the user's crate. Users depend on `djogi`
/// but may not have `heeranjid` or `time` as direct crate-level dependencies.
/// Referencing those through `::djogi::types` avoids the E0433 "crate not found"
/// error — `djogi` re-exports `HeerId`, `RanjId`, `DateTime`, etc. via its
/// `types` module, so a single dependency is all the user ever needs.
fn inject_fields(struct_item: &mut ItemStruct, model_attrs: &ModelAttrs) {
    let id_field: Option<syn::Field> =
        pk_type_tokens(&model_attrs.pk).map(|ty| parse_quote! { pub id: #ty });

    let created_at_field: syn::Field = parse_quote! { pub created_at: ::djogi::types::DateTime };
    let updated_at_field: syn::Field = parse_quote! { pub updated_at: ::djogi::types::DateTime };

    if let syn::Fields::Named(named) = &mut struct_item.fields {
        let user_fields = std::mem::take(&mut named.named);
        if let Some(id) = id_field {
            named.named.push(id);
        }
        named.named.push(created_at_field);
        named.named.push(updated_at_field);
        named.named.extend(user_fields);
    }
}

/// Generate `impl Default for <Struct>` with sentinel values for framework fields.
/// Sentinel values:
/// - `HeerId` / `HeerIdDesc` / `RanjId` / `RanjIdDesc` →
/// `<T as ::djogi::primary_key::PrimaryKey>::sentinel` — zero-valued
/// instance the trait factory produces. Replaces the pre-Phase-7-Zero-2
/// `::djogi::types::__*_default` hidden helpers.
/// - `i32` (serial) → `0i32` (matches `<i32 as PrimaryKey>::sentinel`)
/// - `created_at` / `updated_at` → `::djogi::types::DateTime::UNIX_EPOCH`
/// - User fields → `Default::default` (user types must implement `Default`)
/// The `user_field_defaults` filter operates on the struct's field list
/// *after* `inject_fields` has prepended framework fields. For `pk = None`,
/// no `id` is injected, so a user's own `id` field (if present) survives the
/// filter and gets a `Default::default` entry like any other user field.
fn generate_default_impl(struct_item: &ItemStruct, model_attrs: &ModelAttrs) -> TokenStream {
    let name = &struct_item.ident;
    let (impl_generics, ty_generics, where_clause) = struct_item.generics.split_for_impl();

    // Names that were actually injected (and therefore must be excluded from
    // the user-field default loop because they're initialised explicitly below).
    let user_field_defaults: Vec<TokenStream> = struct_item
        .fields
        .iter()
        .filter(|f| {
            let n = f.ident.as_ref().map(|i| i.to_string()).unwrap_or_default();
            !is_framework_column(&n, model_attrs)
        })
        .map(|f| {
            let fname = &f.ident;
            // Built-in PK-shaped types (HeerId/RanjId families and their
            // recency-biased aliases) come from the upstream `heeranjid`
            // crate and cannot carry `impl Default` here (orphan rule).
            // Route their defaults through `<T as PrimaryKey>::sentinel`
            // so an ambient (non-`id`) field of such a type still
            // initialises in the generated `Default` impl. Custom PK types
            // emitted by `djogi::primary_key!` ship their own `impl Default`
            // delegating to `sentinel`, so they fall through to the
            // default branch without needing a name match.
            if is_builtin_pk_type(&f.ty) {
                let ty = &f.ty;
                quote! {
                    #fname: <#ty as ::djogi::primary_key::PrimaryKey>::sentinel()
                }
            } else {
                quote! { #fname: ::std::default::Default::default() }
            }
        })
        .collect();

    let id_part = pk_type_tokens(&model_attrs.pk)
        .map(|ty| quote! { id: <#ty as ::djogi::primary_key::PrimaryKey>::sentinel(), })
        .unwrap_or_default();

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

/// Tokens for the `id` field's type under each PK strategy, or `None` when
/// the macro should not inject an `id` field at all (`pk = None`).
/// Used by both `inject_fields` (to build the field declaration) and
/// `generate_default_impl` (to build the `<T as PrimaryKey>::sentinel`
/// expression). Custom PK paths are interpolated verbatim — the macro
/// `djogi::primary_key!` ships the matching trait impls at the path's
/// definition site.
fn pk_type_tokens(pk: &PkStrategy) -> Option<TokenStream> {
    Some(match pk {
        PkStrategy::HeerId => quote! { ::djogi::types::HeerId },
        PkStrategy::RanjId => quote! { ::djogi::types::RanjId },
        PkStrategy::HeerIdDesc => quote! { ::djogi::types::HeerIdDesc },
        PkStrategy::RanjIdDesc => quote! { ::djogi::types::RanjIdDesc },
        PkStrategy::Serial => quote! { i32 },
        PkStrategy::None => return None,
        PkStrategy::Custom(path) => quote! { #path },
    })
}

/// Recognise the built-in PK-shaped types — `HeerId` / `HeerIdDesc` /
/// `HeerIdRecencyBiased` and the `RanjId*` family — when they appear as
/// user-declared ambient fields.
/// These types come from the upstream `heeranjid` crate (re-exported via
/// `djogi::types`) and Djogi cannot carry `impl Default` for them (orphan
/// rule). The generated `Default` impl routes ambient fields of such a type
/// through `<T as PrimaryKey>::sentinel` so the impl still compiles.
/// Path forms accepted: bare ident, `djogi::T`, and `djogi::types::T`
/// each with or without a leading `::`. Generic arguments anywhere in the
/// path disqualify the match (PK types are nullary).
fn is_builtin_pk_type(ty: &syn::Type) -> bool {
    let syn::Type::Path(syn::TypePath { path, qself: None }) = ty else {
        return false;
    };
    let segs = &path.segments;
    let Some(last) = segs.last() else {
        return false;
    };

    let is_pk_alias = last.ident == "HeerId"
        || last.ident == "HeerIdDesc"
        || last.ident == "HeerIdRecencyBiased"
        || last.ident == "RanjId"
        || last.ident == "RanjIdDesc"
        || last.ident == "RanjIdRecencyBiased";
    if !is_pk_alias {
        return false;
    }
    if segs
        .iter()
        .any(|seg| !matches!(seg.arguments, syn::PathArguments::None))
    {
        return false;
    }

    match segs.len() {
        1 => true,
        2 => segs[0].ident == "djogi",
        3 => segs[0].ident == "djogi" && segs[1].ident == "types",
        _ => false,
    }
}
