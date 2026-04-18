//! Orchestrates the `#[model]` attribute macro expansion.
//!
//! Each sub-module handles one concern. Modules that are not yet implemented
//! expose a no-op `expand(...)` that returns an empty `TokenStream`, so the
//! overall pipeline compiles in Task 3 and each subsequent task can replace its
//! stub in isolation without touching this file.

pub mod attrs;
pub mod crud;
pub mod descriptor;
pub mod from_row;
pub mod inject;
pub mod stubs;

use attrs::ModelAttrs;
use proc_macro2::TokenStream;
use syn::{ItemStruct, parse2};

/// Called from `lib.rs`. Returns the full expanded token stream, or a
/// compile-error token stream on parse/validation failure.
pub fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    expand_inner(attr, item).unwrap_or_else(|e| e.to_compile_error())
}

fn expand_inner(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let model_attrs = ModelAttrs::parse(attr)?;

    // Parse the struct (all user attributes remain intact).
    let mut struct_item: ItemStruct = parse2(item)?;

    // Collect per-field attribute options before we add framework fields.
    let field_attrs: Vec<attrs::FieldAttrs> = struct_item
        .fields
        .iter()
        .map(attrs::FieldAttrs::parse)
        .collect::<syn::Result<_>>()?;

    // 1. Inject framework fields (`id`, `created_at`, `updated_at`) and emit the
    //    `Default` impl — both are concatenated into a single TokenStream by
    //    `inject::expand`. Returns `syn::Error` for tuple/unit structs and for
    //    user fields that collide with reserved framework names.
    let expanded = inject::expand(&mut struct_item, &model_attrs)?;

    // 2. FromRow impl — Task 5 wires this up.
    let from_row = from_row::expand(&struct_item, &model_attrs, &field_attrs);

    // 3. Model trait impl (CRUD) — Tasks 7–9 wire this up.
    let model_impl = crud::expand(&struct_item, &model_attrs, &field_attrs);

    // 4. ModelDescriptor + inventory::submit! — Task 6 wires this up.
    let descriptor = descriptor::expand(&struct_item, &model_attrs, &field_attrs);

    // 5. {Model}Fields accessors + {Model}Filter stub. `stubs::expand` now
    //    emits real per-column `FieldRef` accessors (Phase 2 Task 4); it needs
    //    `model_attrs` for future pk-aware filter work and reads the field
    //    types directly off the post-injection `struct_item`.
    let stubs = stubs::expand(&struct_item, &model_attrs);

    Ok(quote::quote! {
        #expanded
        #from_row
        #model_impl
        #descriptor
        #stubs
    })
}
