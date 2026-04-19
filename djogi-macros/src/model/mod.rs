//! Orchestrates the `#[model]` attribute macro expansion.
//!
//! Each sub-module handles one concern. Modules that are not yet implemented
//! expose a no-op `expand(...)` that returns an empty `TokenStream`, so the
//! overall pipeline compiles in Task 3 and each subsequent task can replace its
//! stub in isolation without touching this file.

pub mod attrs;
pub mod crud;
pub mod descriptor;
pub mod filter;
pub mod from_joined_row;
pub mod from_row;
pub mod inject;
pub mod relations;
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

    // Strip `#[field(...)]` attributes from user fields. We already captured
    // their semantics into `field_attrs` above; leaving the raw attribute on
    // the struct surface would confuse rustc, which does not recognise
    // `field` as a helper attribute for the `#[model]` attribute macro
    // (helper attributes only exist on `#[derive(...)]` macros — see the
    // `proc_macro_derive(Model, attributes(field))` declaration in lib.rs,
    // which governs a separate no-op `Model` derive). Stripping here keeps
    // the emitted struct valid Rust without forcing users to also write
    // `#[derive(Model)]` solely to legalise the `#[field(...)]` parsing.
    if let syn::Fields::Named(named) = &mut struct_item.fields {
        for field in &mut named.named {
            field.attrs.retain(|a| !a.path().is_ident("field"));
        }
    }

    // 1. Inject framework fields (`id`, `created_at`, `updated_at`) and emit the
    //    `Default` impl — both are concatenated into a single TokenStream by
    //    `inject::expand`. Returns `syn::Error` for tuple/unit structs and for
    //    user fields that collide with reserved framework names.
    let expanded = inject::expand(&mut struct_item, &model_attrs)?;

    // 2. FromRow impl — Task 5 wires this up.
    let from_row = from_row::expand(&struct_item, &model_attrs, &field_attrs);

    // 2b. FromJoinedRow impl — Phase 3 Task 5. Sibling to `from_row` that
    //     accepts a `prefix` parameter; used by `QuerySet::select_related`
    //     to decode both parent (empty prefix) and child
    //     (`"rel_{source_column}."`) from a single joined row. Emitted
    //     for every model so `.select_related(path)` never fails at
    //     compile time for lack of a decoder.
    let from_joined_row = from_joined_row::expand(&struct_item, &model_attrs, &field_attrs);

    // 3. Model trait impl (CRUD) — Tasks 7–9 wire this up.
    let model_impl = crud::expand(&struct_item, &model_attrs, &field_attrs);

    // 4. ModelDescriptor + inventory::submit! — Task 6 wires this up.
    let descriptor = descriptor::expand(&struct_item, &model_attrs, &field_attrs);

    // 5. {Model}Fields — typed closure-API accessors. `stubs::expand` emits
    //    per-column `FieldRef` accessors (Phase 2 Task 4); it needs
    //    `model_attrs` for pk-aware framework-field gating and reads the
    //    field types directly off the post-injection `struct_item`.
    let stubs = stubs::expand(&struct_item, &model_attrs);

    // 6. {Model}Filter — programmatic (closure-free) filter builder.
    //    Separate codegen path: emits a runtime struct carrying a
    //    Vec<FilterClause> with one setter per user field. See
    //    `filter::expand`'s module docs for the typed-vs-erased rationale.
    let filter = filter::expand(&struct_item, &model_attrs, &field_attrs);

    // 7. {Model}Related — typed relation-path constructors (Phase 3 Task 2).
    //    Independent of ModelAttrs/FieldAttrs: the emitter inspects field
    //    types directly via `detect_relation`. Emits a ZST `{Model}Related`
    //    with one method per FK / O2O field — consumed by QuerySet's
    //    prefetch / select_related in Phase 3 Tasks 4 + 5.
    let related = relations::expand(&struct_item);

    Ok(quote::quote! {
        #expanded
        #from_row
        #from_joined_row
        #model_impl
        #descriptor
        #stubs
        #filter
        #related
    })
}
